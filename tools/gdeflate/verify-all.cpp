// Offline verification of every published block using file-backed DirectStorage GPU requests.
// Build against the DirectStorage SDK with dstorage.lib, d3d12.lib, dxgi.lib and bcrypt.lib.
#define NOMINMAX
#include <windows.h>
#include <bcrypt.h>
#include <d3d12.h>
#include <dstorage.h>
#include <dxgi1_6.h>
#include <wrl/client.h>

#include <algorithm>
#include <array>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string_view>
#include <vector>

using Microsoft::WRL::ComPtr;
using Digest = std::array<unsigned char, 32>;
struct Entry {
    Digest digest;
    uint64_t offset;
    uint32_t compressed, decoded;
};
struct DiskEntry {
    Digest source, digest;
    uint64_t offset;
    uint32_t compressed, stored, decoded, flags;
};
static_assert(sizeof(DiskEntry) == 88);

static void check(HRESULT hr, const char* operation) {
    if (FAILED(hr)) {
        std::cerr << operation << " HRESULT 0x" << std::hex << unsigned(hr) << std::dec << '\n';
        throw std::runtime_error(operation);
    }
}

static std::vector<Entry> read_index(const std::filesystem::path& path, uint64_t& capacity) {
    std::ifstream input(path, std::ios::binary);
    char magic[8]{};
    uint64_t table = 0, count = 0;
    input.read(magic, 8);
    input.read(reinterpret_cast<char*>(&table), 8);
    input.read(reinterpret_cast<char*>(&count), 8);
    auto size = std::filesystem::file_size(path);
    if (!input || std::memcmp(magic, "FX1GDF01", 8) || table < 24 || table > size ||
        count == 0 || count > 10000000 || size - table != count * sizeof(DiskEntry))
        throw std::runtime_error("Invalid pack index");
    input.seekg(table);
    std::vector<Entry> records;
    records.reserve(static_cast<size_t>(count));
    DiskEntry r{};
    while (records.size() < count) {
        input.read(reinterpret_cast<char*>(&r), sizeof(r));
        if (!input || !r.decoded || !r.compressed || r.offset < 24 || r.offset > table ||
            r.compressed > table - r.offset || r.decoded > (64u << 20))
            throw std::runtime_error("Invalid block");
        records.push_back({r.digest, r.offset, r.compressed, r.decoded});
        capacity = std::max(capacity, uint64_t(r.decoded));
    }
    std::sort(records.begin(), records.end(), [](auto& a, auto& b) { return a.offset < b.offset; });
    uint64_t end = 24;
    for (const Entry& entry : records) {
        if (entry.offset != end) throw std::runtime_error("Missing or overlapping pack data");
        end += entry.compressed;
    }
    if (end != table) throw std::runtime_error("Unindexed pack data");
    return records;
}

// Replay a private capture in request order. Each line selects an index row and
// the expected pack offset. Expected decoded hashes still come from the pack.
static std::vector<Entry> replay_records(const std::filesystem::path& path,
                                         const std::filesystem::path& selection, uint64_t& capacity) {
    std::ifstream input(path, std::ios::binary), lines(selection);
    char magic[8]{};
    uint64_t table = 0, count = 0;
    input.read(magic, 8);
    input.read(reinterpret_cast<char*>(&table), 8);
    input.read(reinterpret_cast<char*>(&count), 8);
    const auto size = std::filesystem::file_size(path);
    if (!input || !lines || std::memcmp(magic, "FX1GDF01", 8) || table < 24 || table > size ||
        !count || count > 10000000 || size - table != count * sizeof(DiskEntry))
        throw std::runtime_error("Invalid pack or replay selection");
    std::vector<Entry> records;
    uint64_t index, expected;
    while (lines >> std::ws && lines.peek() != std::char_traits<char>::eof()) {
        if (!(lines >> index >> expected) || index >= count)
            throw std::runtime_error("Invalid replay record");
        input.seekg(table + index * sizeof(DiskEntry));
        DiskEntry r{};
        input.read(reinterpret_cast<char*>(&r), sizeof r);
        if (!input || r.offset != expected || !r.decoded || r.decoded > (64u << 20) ||
            !r.compressed || r.offset < 24 || r.offset > table || r.compressed > table - r.offset)
            throw std::runtime_error("Replay record does not match pack");
        capacity = std::max(capacity, uint64_t(r.decoded));
        records.push_back({r.digest, r.offset, r.compressed, r.decoded});
    }
    if (records.empty()) throw std::runtime_error("Empty replay");
    return records;
}

static ComPtr<ID3D12Resource> buffer(ID3D12Device* device, UINT64 capacity, D3D12_HEAP_TYPE type) {
    D3D12_HEAP_PROPERTIES heap{};
    heap.Type = type;
    heap.CreationNodeMask = heap.VisibleNodeMask = 1;
    D3D12_RESOURCE_DESC desc{};
    desc.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
    desc.Width = capacity;
    desc.Height = desc.DepthOrArraySize = desc.MipLevels = desc.SampleDesc.Count = 1;
    desc.Layout = D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    ComPtr<ID3D12Resource> resource;
    check(device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc,
          type == D3D12_HEAP_TYPE_DEFAULT ? D3D12_RESOURCE_STATE_COMMON : D3D12_RESOURCE_STATE_COPY_DEST,
          nullptr, IID_PPV_ARGS(&resource)), "CreateCommittedResource");
    return resource;
}

int wmain(int argc, wchar_t** argv) try {
    const bool replay_args = argc == 5 || argc == 6;
    const bool by_bytes = replay_args && std::wstring_view(argv[2]) == L"--replay-mib";
    const bool replay = by_bytes || (replay_args && std::wstring_view(argv[2]) == L"--replay");
    if (argc != 2 && !replay)
        throw std::runtime_error("Usage: verify-all pack [(--replay list requests | --replay-mib list decoded_MiB) [staging_MiB]]");
    constexpr UINT max_batch = 512;
    UINT batch = max_batch;
    uint64_t capacity = 32u << 20;
    uint64_t byte_limit = 0;
    if (replay) {
        size_t used = 0;
        const auto n = std::stoul(argv[4], &used);
        if (argv[4][used] || !n || n > (by_bytes ? 128u : max_batch))
            throw std::runtime_error("Invalid replay batch size");
        if (by_bytes) {
            byte_limit = uint64_t(n) << 20;
            capacity = byte_limit;
            batch = DSTORAGE_MAX_QUEUE_CAPACITY - 1; // one slot for the completion fence
        } else {
            batch = static_cast<UINT>(n);
        }
    }
    auto records = replay ? replay_records(argv[1], argv[3], capacity) : read_index(argv[1], capacity);
    ComPtr<IDXGIFactory6> dxgi;
    ComPtr<IDXGIAdapter1> adapter;
    ComPtr<ID3D12Device> device;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&dxgi)), "CreateDXGIFactory1");
    check(dxgi->EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
          IID_PPV_ARGS(&adapter)), "EnumAdapterByGpuPreference");
    check(D3D12CreateDevice(adapter.Get(), D3D_FEATURE_LEVEL_12_0, IID_PPV_ARGS(&device)), "D3D12CreateDevice");
    DXGI_ADAPTER_DESC1 ad{};
    check(adapter->GetDesc1(&ad), "GetDesc1");
    std::wcout << ad.Description << std::endl;
    ComPtr<IDStorageFactory> factory;
    ComPtr<IDStorageQueue2> storage;
    ComPtr<IDStorageFile> file;
    check(DStorageGetFactory(IID_PPV_ARGS(&factory)), "DStorageGetFactory");
    if (replay && argc == 6) {
        size_t used = 0;
        const auto mb = std::stoul(argv[5], &used);
        if (argv[5][used] || !mb || mb > 512) throw std::runtime_error("Invalid staging size in MiB");
        check(factory->SetStagingBufferSize(static_cast<UINT32>(mb << 20)), "SetStagingBufferSize");
        std::cout << "staging_MiB=" << mb << std::endl;
    }
    DSTORAGE_QUEUE_DESC q{};
    q.Capacity = by_bytes ? DSTORAGE_MAX_QUEUE_CAPACITY : 2 * max_batch;
    q.SourceType = DSTORAGE_REQUEST_SOURCE_FILE;
    q.Device = device.Get();
    q.Name = "Offline full pack verification";
    check(factory->CreateQueue(&q, IID_PPV_ARGS(&storage)), "CreateQueue");
    auto support = storage->GetCompressionSupport(DSTORAGE_COMPRESSION_FORMAT_GDEFLATE);
    if (!(support & (DSTORAGE_COMPRESSION_SUPPORT_GPU_OPTIMIZED | DSTORAGE_COMPRESSION_SUPPORT_GPU_FALLBACK)) ||
         (support & DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK)) throw std::runtime_error("GPU decompression unavailable");
    check(factory->OpenFile(argv[1], IID_PPV_ARGS(&file)), "OpenFile");
    std::cout << records.size() << " records, GPU compression support 0x" << std::hex << support << std::dec << std::endl;
    D3D12_COMMAND_QUEUE_DESC cq{};
    cq.Type = D3D12_COMMAND_LIST_TYPE_COPY;
    ComPtr<ID3D12CommandQueue> copy;
    ComPtr<ID3D12Fence> fence;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> list;
    check(device->CreateCommandQueue(&cq, IID_PPV_ARGS(&copy)), "CreateCommandQueue");
    check(device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)), "CreateFence");
    check(device->CreateCommandAllocator(cq.Type, IID_PPV_ARGS(&allocator)), "CreateCommandAllocator");
    check(device->CreateCommandList(0, cq.Type, allocator.Get(), nullptr, IID_PPV_ARGS(&list)), "CreateCommandList");
    check(list->Close(), "Close");
    auto gpu = buffer(device.Get(), capacity, D3D12_HEAP_TYPE_DEFAULT);
    auto rb = buffer(device.Get(), capacity, D3D12_HEAP_TYPE_READBACK);
    auto event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!event) throw std::runtime_error("CreateEvent");
    uint64_t value = 0, verifiedBytes = 0;
    std::vector<uint64_t> offsets(batch);
    size_t submit_count = 0;
    uint64_t largest_output = 0;
    auto start = std::chrono::steady_clock::now(), last = start;
    for (size_t i = 0; i < records.size();) {
        uint64_t used = 0;
        size_t n = 0;
        for (; n < batch && i + n < records.size(); ++n) {
            const auto& e = records[i+n];
            auto off = (used + 15) & ~uint64_t(15);
            if (off + e.decoded > capacity || (n && byte_limit && off + e.decoded > byte_limit)) break;
            offsets[n] = off;
            DSTORAGE_REQUEST request{};
            request.Options.SourceType = q.SourceType;
            request.Options.DestinationType = DSTORAGE_REQUEST_DESTINATION_BUFFER;
            request.Options.CompressionFormat = DSTORAGE_COMPRESSION_FORMAT_GDEFLATE;
            request.Source.File = {file.Get(), e.offset, e.compressed};
            request.Destination.Buffer = {gpu.Get(), off, e.decoded};
            request.UncompressedSize = e.decoded;
            storage->EnqueueRequest(&request);
            used = off + e.decoded;
        }
        ++submit_count;
        largest_output = std::max(largest_output, used);
        storage->EnqueueSignal(fence.Get(), ++value);
        storage->Submit();
        check(copy->Wait(fence.Get(), value), "Wait");
        check(allocator->Reset(), "Reset allocator");
        check(list->Reset(allocator.Get(), nullptr), "Reset list");
        list->CopyBufferRegion(rb.Get(), 0, gpu.Get(), 0, used);
        check(list->Close(), "Close");
        ID3D12CommandList* lists[] = {list.Get()};
        copy->ExecuteCommandLists(1, lists);
        check(copy->Signal(fence.Get(), ++value), "Signal");
        check(fence->SetEventOnCompletion(value, event), "SetEventOnCompletion");
        if (WaitForSingleObject(event, 60000) != WAIT_OBJECT_0 || fence->GetCompletedValue() == UINT64_MAX)
            throw std::runtime_error("GPU timeout or removal");
        DSTORAGE_ERROR_RECORD errors{};
        storage->RetrieveErrorRecord(&errors);
        if (errors.FailureCount) throw std::runtime_error("DirectStorage reported a failed request");
        D3D12_RANGE range{0, static_cast<SIZE_T>(used)}, empty{};
        void* mapped = nullptr;
        check(rb->Map(0, &range, &mapped), "Map");
        for (size_t j = 0; j < n; ++j) {
            const auto& e = records[i+j];
            Digest got{};
            check(BCryptHash(BCRYPT_SHA256_ALG_HANDLE, nullptr, 0, static_cast<PUCHAR>(mapped) + offsets[j],
                  e.decoded, got.data(), static_cast<ULONG>(got.size())), "BCryptHash");
            if (got != e.digest) {
                std::cerr << "SHA-256 mismatch at file offset " << e.offset << ", output bytes " << e.decoded << '\n';
                throw std::runtime_error("Incorrect decoded block");
            }
            verifiedBytes += e.decoded;
        }
        rb->Unmap(0, &empty);
        i += n;
        auto now = std::chrono::steady_clock::now();
        if (now - last >= std::chrono::seconds(15) || i == records.size()) {
            std::cout << i << "/" << records.size() << " verified, " << verifiedBytes << " decoded bytes, "
                      << std::chrono::duration_cast<std::chrono::seconds>(now-start).count() << " s" << std::endl;
            last = now;
        }
    }
    if (replay) {
        const auto ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
        std::cout << "replay verified=" << records.size() << " bytes=" << verifiedBytes
                  << " batch=" << batch << " decoded_limit_bytes=" << byte_limit
                  << " fenced_batches=" << submit_count << " largest_output=" << largest_output << " elapsed_ms=" << ms << " mib_s="
                  << double(verifiedBytes) / (1024.0 * 1024.0) / (ms / 1000.0) << std::endl;
    }
    CloseHandle(event);
    return 0;
} catch (const std::exception& e) {
    std::cerr << e.what() << std::endl;
    return 1;
}
