// Optional conversion bridge for title requests that still require CPU-visible bytes.
// One lane by default; VICTORY_GDEFLATE_LANES=2 or 4 permits independent in-flight reads.
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>
#include <dstorage.h>
#include <bcrypt.h>
#include <wrl/client.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstring>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <memory>
#include <mutex>
#include <vector>
using Microsoft::WRL::ComPtr;
using Hash=std::array<unsigned char,32>;
using PrepareWrite = void (*)(void*, uint32_t);
struct Record { Hash source,digest; uint64_t offset; uint32_t compressed,stored,decoded,flags; };
static_assert(sizeof(Record)==88);
static void checked(HRESULT hr){if(FAILED(hr)) throw hr;}
static Hash sha(const void* p,uint32_t n){
    Hash v; checked(BCryptHash(BCRYPT_SHA256_ALG_HANDLE,nullptr,0,(PUCHAR)p,n,v.data(),32));return v;
}
static constexpr UINT capacity=4u<<20;
static std::atomic<uint64_t> reused_readback_bytes{0};
#include "asset_markers.h"
// Only the calling storage workers participate. The first worker arms a
// high-resolution deadline and sleeps until dispatch or expiry. Every other
// participant sleeps on its own lane event. No render-thread or DLL-lifetime
// worker, staged asset payload, or additional GPU allocation is needed.
struct SubmissionBatch {
    using Clock = std::chrono::steady_clock;
    enum Counter { Requests, Batches, Bytes, PeakFill, PeakBytes, SumAge, MaxAge,
        Deadlines, ByteCap, LaneCap, Oversize, LateUs, MaxLateUs, TimerError, Count };
    static inline std::array<std::atomic<uint64_t>, Count> totals{};
    enum Reason { Expired = Deadlines, FullBytes = ByteCap, FullLanes = LaneCap, TimerFailed = TimerError };
    IDStorageQueue2* queue = nullptr;
    UINT ttl_us = 0, budget = 0, lanes = 0, fill = 0, bytes = 0;
    std::array<UINT, 4> pending{};
    std::array<HANDLE, 4> dispatched{}, timers{};
    Clock::time_point started{}, deadline{};

    ~SubmissionBatch() {
        for (auto h : dispatched) if (h) CloseHandle(h);
        for (auto h : timers) if (h) CloseHandle(h);
    }
    static UINT setting(const wchar_t* name, UINT absent, UINT maximum) {
        wchar_t text[12]{};
        SetLastError(ERROR_SUCCESS);
        DWORD n = GetEnvironmentVariableW(name, text, 12);
        if (!n) return GetLastError() == ERROR_ENVVAR_NOT_FOUND ? absent : 0;
        if (n >= 12) return 0;
        UINT value = 0;
        for (DWORD i = 0; i < n; ++i) {
            if (text[i] < L'0' || text[i] > L'9') return 0;
            value = value * 10 + text[i] - L'0';
            if (value > maximum) return 0;
        }
        return value;
    }
    void init(IDStorageQueue2* storage, UINT lane_count) {
        const UINT ttl = setting(L"VICTORY_GDEFLATE_BATCH_US", 0, 2000);
        const UINT kib = setting(L"VICTORY_GDEFLATE_BATCH_KIB", 512, 4096);
        if (!ttl || !kib || lane_count < 2) return;
        for (UINT i = 0; i < lane_count; ++i) {
            dispatched[i] = CreateEventW(nullptr, FALSE, FALSE, nullptr);
            timers[i] = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
            if (!dispatched[i] || !timers[i]) {
                std::fprintf(stderr, "gdeflate batch disabled: high-resolution timer/event unavailable\n");
                return;
            }
        }
        queue = storage;
        ttl_us = ttl;
        budget = kib * 1024;
        lanes = lane_count;
    }
    static void peak(Counter counter, uint64_t value) {
        // All writers hold the queue submission lock.
        if (value > totals[counter].load(std::memory_order_relaxed)) totals[counter].store(value, std::memory_order_relaxed);
    }
    void flush(Reason reason) {
        queue->Submit();
        const auto now = Clock::now();
        const auto age = std::chrono::duration_cast<std::chrono::microseconds>(now - started).count();
        const auto late = std::max<int64_t>(0, std::chrono::duration_cast<std::chrono::microseconds>(now - deadline).count());
        totals[Batches].fetch_add(1, std::memory_order_relaxed);
        totals[reason].fetch_add(1, std::memory_order_relaxed);
        totals[SumAge].fetch_add(age, std::memory_order_relaxed);
        totals[LateUs].fetch_add(late, std::memory_order_relaxed);
        peak(PeakFill, fill); peak(PeakBytes, bytes); peak(MaxAge, age); peak(MaxLateUs, late);
        for (UINT i = 0; i < fill; ++i) SetEvent(dispatched[pending[i]]);
        bytes = fill = 0;
    }
    // DirectStorage's minimum queue has capacity for all four request/status/
    // signal triplets without automatic half-full submission. Flush old work
    // before enqueueing a triplet which would exceed its budget or deadline.
    void before(UINT incoming) {
        if (fill) {
            if (Clock::now() >= deadline) flush(Expired);
            else if (incoming > budget - bytes) flush(FullBytes);
        }
        if (!fill) {
            started = Clock::now();
            deadline = started + std::chrono::microseconds(ttl_us);
        }
    }
    void submit(UINT lane, UINT size, std::unique_lock<std::mutex>& lock) {
        if (size > budget) {
            totals[Oversize].fetch_add(1, std::memory_order_relaxed);
            queue->Submit();
            return;
        }
        const bool first = fill == 0;
        pending[fill++] = lane;
        bytes += size;
        totals[Requests].fetch_add(1, std::memory_order_relaxed);
        totals[Bytes].fetch_add(size, std::memory_order_relaxed);
        const auto now = Clock::now();
        if (now >= deadline) flush(Expired);
        else if (bytes == budget) flush(FullBytes);
        else if (fill == lanes) flush(FullLanes);
        else if (first) {
            // Negative relative time uses 100 ns units. Arm against the
            // original steady deadline, including time spent in Enqueue*.
            LARGE_INTEGER due{};
            due.QuadPart = -std::max<int64_t>(1, std::chrono::duration_cast<std::chrono::nanoseconds>(deadline - now).count() / 100);
            if (!SetWaitableTimer(timers[lane], &due, 0, nullptr, nullptr, FALSE)) flush(TimerFailed);
        }
        lock.unlock();
        HANDLE handles[] = {dispatched[lane], timers[lane]};
        const DWORD wake = WaitForMultipleObjects(first ? 2 : 1, handles, FALSE, INFINITE);
        if (wake != WAIT_OBJECT_0) {
            lock.lock();
            // Another worker may already have submitted this generation and
            // started a new one. Membership is exclusive until decode returns.
            if (std::find(pending.begin(), pending.begin() + fill, lane) != pending.begin() + fill) {
                flush(wake == WAIT_OBJECT_0 + 1 ? Expired : TimerFailed);
            }
            // Its dispatch was signaled under this lock, whether by us
            // or an earlier flusher. Consume it before the lane is reused.
            WaitForSingleObject(dispatched[lane], 0);
            lock.unlock();
        }
        if (first) CancelWaitableTimer(timers[lane]);
    }
    void sample() const {
        static std::atomic<uint64_t> sequence{};
        const auto n = sequence.fetch_add(1, std::memory_order_relaxed) + 1;
        if (n != 1 && n % 128 != 0) return;
        FILETIME ft{}; GetSystemTimeAsFileTime(&ft);
        ULARGE_INTEGER time{}; time.HighPart = ft.dwHighDateTime; time.LowPart = ft.dwLowDateTime;
        std::fprintf(stderr, "gdeflate batch utc_us=%llu ttl_us=%u budget_bytes=%u requests=%llu batches=%llu bytes=%llu peak_fill=%llu peak_bytes=%llu sum_age_us=%llu max_age_us=%llu deadline=%llu byte_cap=%llu lane_cap=%llu oversize_direct=%llu late_us=%llu max_late_us=%llu timer_error=%llu\n",
            time.QuadPart / 10 - 11644473600000000ULL, ttl_us, budget,
            totals[Requests].load(), totals[Batches].load(), totals[Bytes].load(), totals[PeakFill].load(), totals[PeakBytes].load(),
            totals[SumAge].load(), totals[MaxAge].load(), totals[Deadlines].load(), totals[ByteCap].load(), totals[LaneCap].load(),
            totals[Oversize].load(), totals[LateUs].load(), totals[MaxLateUs].load(), totals[TimerError].load());
    }
};
#include "single_submitter.h"
struct SharedPack {
    std::vector<Record> records;
    ComPtr<IDStorageQueue2> storage;
    ComPtr<IDStorageQueue3> fencedStorage;
    ComPtr<IDStorageStatusArray> statuses;
    ComPtr<IDStorageFile> file;
    std::mutex submission;
    SubmissionBatch batch;
    std::unique_ptr<SingleSubmitter> single;
    UINT support = 0;
    std::atomic<uint64_t> request_number{0}, delivered{0}, reused{0}, evicted{0};
    bool init(ID3D12Device* dev, const wchar_t* path, UINT lanes);
    const Record* find(const Hash& hash, uint32_t stored, uint32_t decoded, uint32_t flags) const {
        auto it = std::lower_bound(records.begin(), records.end(), hash,
            [](const Record& r, const Hash& key) { return r.source < key; });
        for (; it != records.end() && it->source == hash; ++it)
            if (it->stored == stored && it->decoded == decoded && it->flags == flags) return &*it;
        return nullptr;
    }
};
struct Lane {
    SharedPack* pack = nullptr;
    UINT index = 0;
    ComPtr<ID3D12CommandQueue> copy;
    ComPtr<ID3D12Fence> fence;ComPtr<ID3D12Resource> gpu,readback;
    ComPtr<ID3D12Heap> backing;ComPtr<ID3D12CommandAllocator> allocator;ComPtr<ID3D12GraphicsCommandList> commands;
    static constexpr UINT tile_bytes = 65536;
    static constexpr UINT tile_count = capacity / tile_bytes;
    // An entry lives at the first tile of its contiguous span. Pointers refer
    // to the immutable pack index; the lane mutex protects both buffers.
    std::array<const Record*, tile_count> resident{};
    std::array<MarkerRequest, tile_count> admissions{};
    std::array<uint64_t, tile_count> touched{};
    uint64_t touch_clock = 0;
    UINT next_tile = 0;
    bool rolling = false;
    bool lru = false;
    bool trace_cache = false;
    void note_delivery(bool hit) {
        if (!trace_cache) return;
        const auto delivered = pack->delivered.fetch_add(1, std::memory_order_relaxed) + 1;
        if (hit) pack->reused.fetch_add(1, std::memory_order_relaxed);
        if ((delivered % 128) != 0 && delivered != 1) return;
        FILETIME time{};
        GetSystemTimeAsFileTime(&time);
        ULARGE_INTEGER ticks{};
        ticks.LowPart = time.dwLowDateTime;
        ticks.HighPart = time.dwHighDateTime;
        const auto utc_ms = ticks.QuadPart / 10000 - 11644473600000ULL;
        std::fprintf(stderr, "gdeflate cache utc_ms=%llu deliveries=%llu reused=%llu reused_bytes=%llu evicted=%llu\n",
                     utc_ms, delivered, pack->reused.load(std::memory_order_relaxed),
                     reused_readback_bytes.load(std::memory_order_relaxed), pack->evicted.load(std::memory_order_relaxed));
    }
    UINT mapped=0;UINT64 value=0;
    HANDLE event=nullptr;
    ~Lane(){if(event) CloseHandle(event);}
    void init(ID3D12Device* dev, SharedPack& shared, UINT lane_index) {
        pack = &shared;
        index = lane_index;
        wchar_t flag[2]{};
        rolling = GetEnvironmentVariableW(L"VICTORY_GDEFLATE_ROLLING", flag, 2) == 1 && flag[0] == L'1';
        lru = GetEnvironmentVariableW(L"VICTORY_GDEFLATE_LRU", flag, 2) == 1 && flag[0] == L'1';
        trace_cache = GetEnvironmentVariableW(L"VICTORY_GDEFLATE_CACHE_METER", flag, 2) == 1 && flag[0] == L'1';
        D3D12_RESOURCE_DESC b{};b.Dimension=D3D12_RESOURCE_DIMENSION_BUFFER;b.Width=capacity;b.Height=1;b.DepthOrArraySize=b.MipLevels=1;b.SampleDesc.Count=1;b.Layout=D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
        D3D12_HEAP_DESC h{};h.SizeInBytes=capacity;h.Properties.Type=D3D12_HEAP_TYPE_DEFAULT;h.Properties.VisibleNodeMask=h.Properties.CreationNodeMask=1;h.Flags=D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS;
        // Committed slots in single-owner mode need no tile mapping queue or
        // extra per-slot fence. Both modes reserve the same 4 MiB slot budget.
        if (pack->single) {
            checked(dev->CreateCommittedResource(&h.Properties,D3D12_HEAP_FLAG_NONE,&b,D3D12_RESOURCE_STATE_COMMON,nullptr,IID_PPV_ARGS(&gpu)));
        } else {
            D3D12_COMMAND_QUEUE_DESC cq{};cq.Type=D3D12_COMMAND_LIST_TYPE_COPY;checked(dev->CreateCommandQueue(&cq,IID_PPV_ARGS(&copy)));
            checked(dev->CreateFence(0,D3D12_FENCE_FLAG_NONE,IID_PPV_ARGS(&fence)));
            event=CreateEventW(nullptr,FALSE,FALSE,nullptr);if(!event)throw E_FAIL;
            checked(dev->CreateCommandAllocator(cq.Type,IID_PPV_ARGS(&allocator)));
            checked(dev->CreateCommandList(0,cq.Type,allocator.Get(),nullptr,IID_PPV_ARGS(&commands)));checked(commands->Close());
            checked(dev->CreateReservedResource(&b,D3D12_RESOURCE_STATE_COMMON,nullptr,IID_PPV_ARGS(&gpu)));
            checked(dev->CreateHeap(&h,IID_PPV_ARGS(&backing)));
        }
        h.Properties.Type=D3D12_HEAP_TYPE_READBACK;checked(dev->CreateCommittedResource(&h.Properties,D3D12_HEAP_FLAG_NONE,&b,D3D12_RESOURCE_STATE_COPY_DEST,nullptr,IID_PPV_ARGS(&readback)));
    }
    void wait(UINT64 number){
        checked(fence->SetEventOnCompletion(number,event));
        if(WaitForSingleObject(event,10000)!=WAIT_OBJECT_0||fence->GetCompletedValue()==UINT64_MAX)throw DXGI_ERROR_DEVICE_HUNG;
    }
    void deliver(const Record& r,UINT offset,void* dst,PrepareWrite prepare,bool verify){
        D3D12_RANGE range{offset,offset+r.decoded},empty{};
        void* bytes=nullptr;
        checked(readback->Map(0,&range,&bytes));
        try {
            auto* payload = static_cast<const unsigned char*>(bytes) + offset;
            if(verify && sha(payload,r.decoded)!=r.digest) throw HRESULT_FROM_WIN32(ERROR_CRC);
            if(prepare)prepare(dst,r.decoded);
            memcpy(dst,payload,r.decoded);
        }catch(...){
            readback->Unmap(0,&empty);
            throw;
        }
        readback->Unmap(0,&empty);
    }
    static UINT span(const Record& r) { return (r.decoded + tile_bytes - 1) / tile_bytes; }
    void evict(UINT i) {
        if (markers.enabled()) markers.write("eviction", {admissions[i], resident[i], index * tile_count + i});
        resident[i] = nullptr;
        if (trace_cache) pack->evicted.fetch_add(1, std::memory_order_relaxed);
    }
    // Find a contiguous free run, evicting the least recently accessed span
    // one at a time only when fragmentation or occupancy requires it.
    UINT allocate_lru(UINT needed) {
        for (;;) {
            UINT free_start = 0;
            UINT oldest = tile_count;
            for (UINT i = 0; i < tile_count; ++i) {
                if (!resident[i]) continue;
                if (i - free_start >= needed) return free_start;
                free_start = i + span(*resident[i]);
                if (oldest == tile_count || touched[i] < touched[oldest]) oldest = i;
            }
            if (tile_count - free_start >= needed) return free_start;
            evict(oldest);
        }
    }
    void note_verified(const MarkerEvent& event, bool defer) {
        if (!event.request.number) return;
        if (defer) awaiting_destination = event;
        else markers.write("verified_delivery", event);
    }
    int decode(const Record& r,void* dst,PrepareWrite prepare,const MarkerSource* origin){
        MarkerRequest current{};
        if (markers.enabled()) {
            current.number = pack->request_number.fetch_add(1, std::memory_order_relaxed) + 1;
            current.source = origin ? *origin : MarkerSource{0, 0, 0, 2, 0};
        }
        const UINT slots = rolling || lru ? tile_count : 1;
        for (UINT i = 0; i < slots; ++i) {
            if (resident[i] == &r) {
                deliver(r, i * tile_bytes, dst, prepare, false);
                if (lru) touched[i] = ++touch_clock;
                if (current.number) {
                    const MarkerEvent event{current, &r, index * tile_count + i};
                    markers.write("reuse", event);
                    note_verified(event, origin != nullptr);
                }
                reused_readback_bytes.fetch_add(r.decoded, std::memory_order_relaxed);
                note_delivery(true);
                return 1;
            }
        }

        const UINT needed = span(r);
        const UINT first = lru ? allocate_lru(needed) :
                           rolling && next_tile + needed <= tile_count ? next_tile : 0;
        const UINT tiles = first + needed;
        const UINT offset = first * tile_bytes;
        // Invalidate every old span intersecting the incoming write. A block
        // only becomes reusable after GPU completion and digest verification.
        for (UINT i = 0; i < tiles; ++i) {
            if (resident[i] && i + span(*resident[i]) > first) evict(i);
        }
        DSTORAGE_REQUEST req{};req.Options.SourceType=DSTORAGE_REQUEST_SOURCE_FILE;req.Options.DestinationType=DSTORAGE_REQUEST_DESTINATION_BUFFER;req.Options.CompressionFormat=DSTORAGE_COMPRESSION_FORMAT_GDEFLATE;
        req.Source.File={pack->file.Get(),r.offset,r.compressed};req.Destination.Buffer={gpu.Get(),offset,r.decoded};req.UncompressedSize=r.decoded;
        if (pack->single) {
            SingleSubmitter::Job job{};
            job.request = req;
            job.readback = readback.Get();
            job.slot = index;
            job.offset = offset;
            job.bytes = r.decoded;
            pack->single->stage(job);
            if (current.number) markers.write("gpu_submit", {current, &r, index * tile_count + first});
        } else {
            UINT64 mappingReady=0;
            if(tiles>mapped){
                D3D12_TILED_RESOURCE_COORDINATE start{mapped,0,0,0};UINT n=tiles-mapped;
                D3D12_TILE_REGION_SIZE region{n,FALSE,0,0,0};D3D12_TILE_RANGE_FLAGS flags=D3D12_TILE_RANGE_FLAG_NONE;
                copy->UpdateTileMappings(gpu.Get(),1,&start,&region,backing.Get(),1,&flags,&mapped,&n,D3D12_TILE_MAPPING_FLAG_NONE);
                checked(copy->Signal(fence.Get(),++value));
                mappingReady=value;
                mapped=tiles;
            }
            if (mappingReady && !pack->fencedStorage) wait(mappingReady);
            {
                // Status reports failures since the preceding status entry on this
                // shared queue. Keep request/status/signal adjacent, then release
                // before any fence waits, readback, verification or guest writes.
                std::unique_lock lock(pack->submission);
                if (pack->batch.ttl_us) pack->batch.before(r.decoded);
                if(mappingReady && pack->fencedStorage) {
                    pack->fencedStorage->EnqueueRequests(&req,1,fence.Get(),mappingReady,DSTORAGE_ENQUEUE_REQUEST_FLAG_FENCE_WAIT_BEFORE_GPU_WORK);
                } else {
                    pack->storage->EnqueueRequest(&req);
                }
                pack->storage->EnqueueStatus(pack->statuses.Get(), index);
                pack->storage->EnqueueSignal(fence.Get(),++value);
                if (pack->batch.ttl_us) pack->batch.submit(index, r.decoded, lock);
                else pack->storage->Submit();
            }
            if (pack->batch.ttl_us) pack->batch.sample();
            if (current.number) markers.write("gpu_submit", {current, &r, index * tile_count + first});
            checked(copy->Wait(fence.Get(),value));checked(allocator->Reset());checked(commands->Reset(allocator.Get(),nullptr));
            commands->CopyBufferRegion(readback.Get(),offset,gpu.Get(),offset,r.decoded);checked(commands->Close());
            ID3D12CommandList* batch[]={commands.Get()};copy->ExecuteCommandLists(1,batch);checked(copy->Signal(fence.Get(),++value));wait(value);
            checked(pack->statuses->GetHResult(index));
        }
        deliver(r,offset,dst,prepare,true);
        resident[first] = &r;
        if (lru) touched[first] = ++touch_clock;
        if (current.number) {
            admissions[first] = current;
            note_verified({current, &r, index * tile_count + first}, origin != nullptr);
        }
        next_tile = tiles;
        note_delivery(false);
        return 1;
    }
};
// The detailed ABI distinguishes ordinary bypasses; legacy exports map them back to zero.
enum ReadBypass {
    Busy = 2, IndexMiss = 3, DisabledAfterError = 4, InvalidInput = 5,
    SizeOutsideLane = 6, NoGpuCompression = 7,
};
static int legacy_status(int status) noexcept { return status > 1 ? 0 : status; }

// Only storage workers enter this pool. Two waiters can retain at most 4 MiB
// each of stored input and 4 MiB each of destination/scratch. The bridge
// allocates no per-waiter payload or GPU resources.
static constexpr UINT waiter_limit = 2;
struct SlotWaitStats {
    std::atomic<uint64_t> queued{}, admitted{}, timeouts{}, overflow{}, peak{}, elapsed_us{}, max_us{}, gate_busy{};
    std::atomic<uint64_t> current{};
    void maximum(std::atomic<uint64_t>& counter, uint64_t value) {
        auto prior = counter.load(std::memory_order_relaxed);
        while (prior < value && !counter.compare_exchange_weak(prior, value, std::memory_order_relaxed)) {}
    }
    void sample() {
        static std::atomic<uint64_t> samples{0};
        const auto serial = samples.fetch_add(1, std::memory_order_relaxed) + 1;
        if (serial != 1 && serial % 128 != 0) return;
        FILETIME time{};
        GetSystemTimeAsFileTime(&time);
        ULARGE_INTEGER ticks{};
        ticks.HighPart = time.dwHighDateTime;
        ticks.LowPart = time.dwLowDateTime;
        std::fprintf(stderr, "gdeflate slot_wait utc_us=%llu queued=%llu admitted=%llu timeouts=%llu overflow=%llu peak=%llu elapsed_us=%llu max_us=%llu gate_busy=%llu\n",
            ticks.QuadPart / 10 - 11644473600000000ULL, queued.load(), admitted.load(), timeouts.load(), overflow.load(), peak.load(),
            elapsed_us.load(), max_us.load(), gate_busy.load());
    }
};
static SlotWaitStats slot_wait;

static UINT adapter_vendor(ID3D12Device* device) {
    ComPtr<IDXGIFactory4> factory;
    ComPtr<IDXGIAdapter1> adapter;
    DXGI_ADAPTER_DESC1 desc{};
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))) ||
        FAILED(factory->EnumAdapterByLuid(device->GetAdapterLuid(), IID_PPV_ARGS(&adapter))) ||
        FAILED(adapter->GetDesc1(&desc))) return 0;
    return desc.VendorId;
}

bool SharedPack::init(ID3D12Device* dev, const wchar_t* path, UINT lanes) {
    std::ifstream input(std::filesystem::path(path),std::ios::binary);
    char magic[8];uint64_t table=0,count=0;input.read(magic,8);input.read((char*)&table,8);input.read((char*)&count,8);
    input.seekg(0,std::ios::end);auto length=input.tellg();
    if(!input||memcmp(magic,"FX1GDF01",8)||length<24||table<24||table>(uint64_t)length||count>10000000||count*sizeof(Record)!=(uint64_t)length-table)throw E_INVALIDARG;
    records.resize((size_t)count);input.seekg(table);input.read((char*)records.data(),count*sizeof(Record));
    if(!input)throw E_FAIL;
    for(const auto& r:records) if(r.offset<24||r.offset>table||r.compressed>table-r.offset)throw E_INVALIDARG;
    if(!std::is_sorted(records.begin(),records.end(),[](auto& a,auto& b){return a.source<b.source;}))throw E_INVALIDARG;
    const bool forceShader = SubmissionBatch::setting(L"VICTORY_GDEFLATE_FORCE_SHADER", 0, 1) == 1;
    if (forceShader) {
        DSTORAGE_CONFIGURATION configuration{};
        configuration.DisableGpuDecompressionMetacommand = TRUE;
        checked(DStorageSetConfiguration(&configuration));
    }
    ComPtr<IDStorageFactory> factory;
    checked(DStorageGetFactory(IID_PPV_ARGS(&factory)));
    DSTORAGE_QUEUE_DESC q{};
    q.Capacity = DSTORAGE_MIN_QUEUE_CAPACITY;
    q.SourceType = DSTORAGE_REQUEST_SOURCE_FILE;
    q.Device = dev;
    q.Name = "Neptune GDeflate";
    checked(factory->CreateQueue(&q, IID_PPV_ARGS(&storage)));
    support = storage->GetCompressionSupport(DSTORAGE_COMPRESSION_FORMAT_GDEFLATE);
    const bool optimized = (support & DSTORAGE_COMPRESSION_SUPPORT_GPU_OPTIMIZED) != 0;
    const bool shader = (support & DSTORAGE_COMPRESSION_SUPPORT_GPU_FALLBACK) != 0;
    const bool useGpu = !(support & DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK) &&
                        (forceShader ? shader && !optimized : optimized || shader);
    const UINT vendor = adapter_vendor(dev);
    if (!useGpu) {
        std::fprintf(stderr, "gdeflate backend=original_cpu support=0x%x vendor=0x%x force_shader=%d\n",
                     support, vendor, forceShader);
        return false;
    }
    storage.As(&fencedStorage);
    if (!single) batch.init(storage.Get(), lanes);
    checked(factory->OpenFile(path,IID_PPV_ARGS(&file)));
    checked(factory->CreateStatusArray(lanes, "fx1 decode lanes", IID_PPV_ARGS(&statuses)));
    const char* backend = optimized ? (vendor == 0x10de ? "rtx_io" : "driver_optimized") : "directstorage_shader";
    std::fprintf(stderr, "gdeflate backend=%s support=0x%x vendor=0x%x queue3=%d force_shader=%d\n",
                 backend, support, vendor, fencedStorage.Get() != nullptr, forceShader);
    return true;
}

struct DecoderPool {
    static constexpr UINT maximum = 4;
    SharedPack shared;
    std::array<Lane, maximum> lanes;
    UINT count = 1;
    ComPtr<ID3D12Device> device;
    std::wstring path;
    std::mutex gate;
    std::condition_variable released;
    UINT wait_us = 0, waiters = 0;
    std::atomic<bool> closed{false};
    // The selector only inspects this published snapshot. A lane alone modifies
    // its actual residency until it finishes. Busy resident keys conservatively
    // fall back; concurrent requests never create two native copies of one key.
    std::array<const Record*, maximum> active{};
    std::array<std::array<const Record*, Lane::tile_count>, maximum> published{};
    std::atomic<UINT> occupied{0};
    std::atomic<bool> failed{false};

    bool init(ID3D12Device* dev, const wchar_t* name) {
        wchar_t flag[2]{};
        if (SubmissionBatch::setting(L"VICTORY_GDEFLATE_SINGLE_SUBMIT", 0, 1) == 1) {
            shared.single = std::make_unique<SingleSubmitter>();
            count = 4;
        }
        if (GetEnvironmentVariableW(L"VICTORY_GDEFLATE_LANES", flag, 2) == 1 &&
            (flag[0] == L'2' || flag[0] == L'4' || (shared.single && flag[0] == L'1')))
            count = flag[0] - L'0';
        wchar_t budget[8]{};
        const DWORD length = GetEnvironmentVariableW(L"VICTORY_GDEFLATE_WAIT_US", budget, 8);
        if (length > 0 && length < 8) {
            wchar_t* end = nullptr;
            const auto value = std::wcstoul(budget, &end, 10);
            if (end == budget + length && value >= 1 && value <= 2000) wait_us = static_cast<UINT>(value);
        }
        device = dev;
        path = name;
        if (!shared.init(dev, name, count)) return false;
        for (UINT i = 0; i < count; ++i) lanes[i].init(dev, shared, i);
        markers.open();
        if (shared.single) shared.single->start(dev,shared.storage.Get(),shared.statuses.Get(),count);
        return true;
    }
    // Called only under gate. Wakeups must repeat the full affinity check: an
    // active duplicate or a busy lane holding this resident key cannot migrate.
    UINT claim_locked(const Record* record) {
        if (closed.load(std::memory_order_relaxed)) return count;
        UINT choice = count;
        for (UINT i = 0; i < count; ++i) {
            if (active[i] == record) return count;
            if (std::find(published[i].begin(), published[i].end(), record) != published[i].end()) {
                if (active[i]) return count;
                choice = i;
                break;
            }
            if (!active[i] && choice == count) choice = i;
        }
        if (choice != count) {
            active[choice] = record;
            occupied.fetch_add(1, std::memory_order_relaxed);
        }
        return choice;
    }
    void close_single() {
        if (!shared.single) return;
        std::unique_lock lock(gate);
        closed.store(true, std::memory_order_release);
        released.notify_all();
        released.wait(lock, [&] { return occupied.load(std::memory_order_relaxed) == 0; });
        lock.unlock();
        shared.single->stop();
    }
    UINT claim(const Record* record) {
        std::lock_guard lock(gate);
        return claim_locked(record);
    }
    UINT claim_bounded(const Record* record, bool may_wait, bool& timed_out) {
        using Clock = std::chrono::steady_clock;
        std::unique_lock lock(gate, std::try_to_lock);
        if (!lock.owns_lock()) {
            slot_wait.gate_busy.fetch_add(1, std::memory_order_relaxed);
            slot_wait.sample();
            return count;
        }
        UINT lane = claim_locked(record);
        if (lane < count || !may_wait || failed.load(std::memory_order_acquire)) return lane;
        if (waiters == waiter_limit || record->stored > capacity) {
            slot_wait.overflow.fetch_add(1, std::memory_order_relaxed);
            lock.unlock();
            slot_wait.sample();
            return count;
        }
        const auto started = Clock::now();
        const auto deadline = started + std::chrono::microseconds(wait_us);
        ++waiters;
        slot_wait.current.store(waiters, std::memory_order_relaxed);
        slot_wait.maximum(slot_wait.peak, waiters);
        slot_wait.queued.fetch_add(1, std::memory_order_relaxed);
        do {
            released.wait_until(lock, deadline);
            // A Windows timer wake or mutex reacquisition may run late. Never
            // start new GPU work after the deadline for this wait.
            if (Clock::now() >= deadline) {
                timed_out = true;
                slot_wait.timeouts.fetch_add(1, std::memory_order_relaxed);
                break;
            }
            if (failed.load(std::memory_order_acquire)) break;
            lane = claim_locked(record);
        } while (lane == count);
        slot_wait.current.store(--waiters, std::memory_order_relaxed);
        if (lane < count) slot_wait.admitted.fetch_add(1, std::memory_order_relaxed);
        const auto elapsed = std::chrono::duration_cast<std::chrono::microseconds>(Clock::now() - started).count();
        slot_wait.elapsed_us.fetch_add(elapsed, std::memory_order_relaxed);
        slot_wait.maximum(slot_wait.max_us, elapsed);
        lock.unlock();
        slot_wait.sample();
        return lane;
    }
    void release(UINT index) {
        bool wake = false;
        {
            std::lock_guard lock(gate);
            published[index] = lanes[index].resident;
            active[index] = nullptr;
            occupied.fetch_sub(1, std::memory_order_relaxed);
            wake = waiters != 0 || closed.load(std::memory_order_relaxed);
        }
        if (wake) released.notify_all();
    }
    int read(const void* key, bool supplied_digest, uint32_t stored, uint32_t decoded, uint32_t flags,
             void* dst, uint32_t* support, PrepareWrite prepare, const MarkerSource* origin) {
        // Single-lane treatment retains the early busy bypass before hashing.
        std::unique_lock single(gate, std::defer_lock);
        if (!wait_us && count == 1 && !shared.single && !single.try_lock()) return Busy;
        if (closed.load(std::memory_order_acquire)) return DisabledAfterError;
        if (failed.load(std::memory_order_acquire)) return DisabledAfterError;
        if (!wait_us && count > 1 && occupied.load(std::memory_order_relaxed) == count) return Busy;
        // After a source-offset timeout, HLE can retry the same block using its
        // original bytes on this same worker. Give that retry only a fast claim.
        static thread_local const Record* expired_digest = nullptr;
        const Record* previous = expired_digest;
        expired_digest = nullptr;
        Hash hash;
        if (supplied_digest) memcpy(hash.data(), key, hash.size());
        else hash = sha(key, stored);
        const Record* record = shared.find(hash, stored, decoded, flags);
        if (!record) { *support = shared.support; return IndexMiss; }
        bool timed_out = false;
        const UINT index = wait_us ? claim_bounded(record, supplied_digest || previous != record, timed_out)
                                   : (count == 1 && !shared.single) ? 0 : claim(record);
        if (index == count) {
            if (supplied_digest && timed_out) expired_digest = record;
            return failed.load(std::memory_order_acquire) ? DisabledAfterError : Busy;
        }
        struct Lease {
            DecoderPool& pool; UINT index;
            ~Lease() { if (pool.count > 1 || pool.wait_us || pool.shared.single) pool.release(index); }
        } lease{*this, index};
        if (failed.load(std::memory_order_acquire)) return DisabledAfterError;
        *support = shared.support;
        try {
            return lanes[index].decode(*record, dst, prepare, origin);
        } catch (...) {
            failed.store(true, std::memory_order_release);
            throw;
        }
    }
};

static std::mutex initialization;
static std::atomic<DecoderPool*> published{nullptr};
extern "C" __declspec(dllexport) void fx1_shutdown_single_submit_v1() noexcept {
    std::lock_guard lock(initialization);
    if (auto* pool = published.load(std::memory_order_acquire)) pool->close_single();
}
extern "C" __declspec(dllexport) void fx1_single_submit_counters_v1(uint64_t* values, uint32_t count) noexcept {
    if (!values || count != SingleSubmitter::Count) return;
    for (UINT i = 0; i < count; ++i) values[i] = SingleSubmitter::totals[i].load(std::memory_order_relaxed);
}
static int read_verified(ID3D12Device* device,const wchar_t* pack,const void* source,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support,PrepareWrite prepare,const MarkerSource* origin = nullptr,bool supplied_digest = false) noexcept {
    if(!decoded||decoded>capacity)return SizeOutsideLane;
    if(!device||!pack||!source||!dst||!support||!stored)return InvalidInput;
    *support = 0;
    static std::atomic<bool> failed{false};
    static std::atomic<UINT> refusedSupport{UINT_MAX};
    const auto refused = refusedSupport.load(std::memory_order_acquire);
    if (refused != UINT_MAX) {
        *support = refused;
        return NoGpuCompression;
    }
    if (failed.load(std::memory_order_acquire)) return DisabledAfterError;
    try{
        auto* pool = published.load(std::memory_order_acquire);
        if (!pool) {
            std::unique_lock lock(initialization, std::try_to_lock);
            if (!lock.owns_lock()) return Busy;
            const auto refusedUnderLock = refusedSupport.load(std::memory_order_relaxed);
            if (refusedUnderLock != UINT_MAX) {
                *support = refusedUnderLock;
                return NoGpuCompression;
            }
            if (failed.load(std::memory_order_acquire)) return DisabledAfterError;
            pool = published.load(std::memory_order_acquire);
            if (!pool) {
                auto next = std::make_unique<DecoderPool>();
                try {
                    if (!next->init(device, pack)) {
                        *support = next->shared.support;
                        refusedSupport.store(*support, std::memory_order_release);
                        return NoGpuCompression;
                    }
                }
                catch (...) { failed.store(true, std::memory_order_release); throw; }
                pool = next.release();
                published.store(pool, std::memory_order_release);
            }
        }
        if (device != pool->device.Get() || pool->path != pack) return InvalidInput;
        return pool->read(source,supplied_digest,stored,decoded,flags,dst,support,prepare,origin);
    }catch(HRESULT hr){failed.store(true, std::memory_order_release);return FAILED(hr)?hr:E_FAIL;}
    catch(...){failed.store(true, std::memory_order_release);return E_FAIL;}
}
extern "C" __declspec(dllexport) int fx1_read_gdeflate(ID3D12Device* device,const wchar_t* pack,const void* source,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support) noexcept {
    return legacy_status(read_verified(device,pack,source,stored,decoded,flags,dst,support,nullptr));
}
// The storage worker opens the title's write guard only after the requested block
// has decoded and passed its digest. A busy lane or lookup miss never touches it.
extern "C" __declspec(dllexport) int fx1_read_gdeflate_into_title(ID3D12Device* device,const wchar_t* pack,const void* source,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support,PrepareWrite prepare) noexcept {
    if(!prepare)return E_INVALIDARG;
    return legacy_status(read_verified(device,pack,source,stored,decoded,flags,dst,support,prepare));
}

extern "C" __declspec(dllexport) uint64_t fx1_reused_readback_bytes() noexcept {
    return reused_readback_bytes.load(std::memory_order_relaxed);
}

extern "C" __declspec(dllexport) void fx1_batch_counters_v1(uint64_t* values, uint32_t count) noexcept {
    if (!values || count != SubmissionBatch::Count) return;
    for (UINT i = 0; i < count; ++i) values[i] = SubmissionBatch::totals[i].load(std::memory_order_relaxed);
}
extern "C" __declspec(dllexport) void fx1_slot_wait_counters_v1(uint64_t* values, uint32_t count) noexcept {
    if (!values || count != 9) return;
    const uint64_t snapshot[] = {slot_wait.queued.load(), slot_wait.admitted.load(), slot_wait.timeouts.load(),
        slot_wait.overflow.load(), slot_wait.peak.load(), slot_wait.elapsed_us.load(), slot_wait.max_us.load(),
        slot_wait.gate_busy.load(), slot_wait.current.load()};
    std::copy(std::begin(snapshot), std::end(snapshot), values);
}

extern "C" __declspec(dllexport) int fx1_read_gdeflate_profiled(ID3D12Device* device,const wchar_t* pack,const void* source,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support,PrepareWrite prepare,const MarkerSource* origin) noexcept {
    awaiting_destination = {};
    if (!origin || origin->file > 1) return E_INVALIDARG;
    return legacy_status(read_verified(device,pack,source,stored,decoded,flags,dst,support,prepare,origin));
}

extern "C" __declspec(dllexport) int fx1_read_gdeflate_detailed_v1(ID3D12Device* device,const wchar_t* pack,const void* source,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support,PrepareWrite prepare,const MarkerSource* origin) noexcept {
    awaiting_destination = {};
    if (origin && origin->file > 1) return InvalidInput;
    return read_verified(device,pack,source,stored,decoded,flags,dst,support,prepare,origin);
}

// For a caller that separately verified the immutable source file's identity
// and range against its local manifest. Decoded output is still digest-checked.
extern "C" __declspec(dllexport) int fx1_read_gdeflate_digest_v1(ID3D12Device* device,const wchar_t* pack,const void* digest32,uint32_t stored,uint32_t decoded,uint32_t flags,void* dst,uint32_t* support,PrepareWrite prepare,const MarkerSource* origin) noexcept {
    awaiting_destination = {};
    if (origin && origin->file > 1) return InvalidInput;
    return read_verified(device,pack,digest32,stored,decoded,flags,dst,support,prepare,origin,true);
}

// Called synchronously on the same worker after copying to the guest destination.
extern "C" __declspec(dllexport) void fx1_finish_gdeflate_delivery(int success) noexcept {
    if (!awaiting_destination.record) return;
    markers.write(success ? "verified_delivery" : "delivery_failed", awaiting_destination);
    awaiting_destination = {};
}
