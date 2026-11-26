// Hardware integration test. Each feedback-selected tile receives its own GDeflate block.
// The caller supplies its independently verified SHA-256 and the compiled feedback shader.
#include <windows.h>
#include <d3d12.h>
#include <dxgi1_6.h>
#include <dstorage.h>
#include <wrl/client.h>
#include <bcrypt.h>
#include <bit>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <iomanip>
#include <sstream>
#include <stdexcept>
#include <vector>
using Microsoft::WRL::ComPtr;
using namespace std;
static void check(HRESULT hr, const char* op) {
    if (FAILED(hr)) { ostringstream e; e << op << " HRESULT 0x" << hex << unsigned(hr); throw runtime_error(e.str()); }
}
#define OK(call) check((call), #call)
static string digest(const unsigned char* bytes, ULONG n) {
    UCHAR hash[32]; OK(BCryptHash(BCRYPT_SHA256_ALG_HANDLE,nullptr,0,const_cast<PUCHAR>(bytes),n,hash,sizeof(hash)));
    ostringstream s; for(auto b:hash) s << hex << setfill('0') << setw(2) << unsigned(b); return s.str();
}
static D3D12_HEAP_PROPERTIES props(D3D12_HEAP_TYPE type) {
    D3D12_HEAP_PROPERTIES p{}; p.Type=type; p.CreationNodeMask=p.VisibleNodeMask=1; return p;
}
static void transition(ID3D12GraphicsCommandList* cmd,ID3D12Resource* r,D3D12_RESOURCE_STATES a,D3D12_RESOURCE_STATES b) {
    D3D12_RESOURCE_BARRIER v{};v.Type=D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;v.Transition={r,D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,a,b};cmd->ResourceBarrier(1,&v);
}
static void waitFor(ID3D12Fence* f,UINT64 n) {
    HANDLE e=CreateEventW(nullptr,FALSE,FALSE,nullptr);
    if(!e) throw runtime_error("CreateEvent failed");
    auto hr=f->SetEventOnCompletion(n,e); DWORD w=SUCCEEDED(hr)?WaitForSingleObject(e,30000):WAIT_FAILED; CloseHandle(e);
    OK(hr); if(w!=WAIT_OBJECT_0 || f->GetCompletedValue()==UINT64_MAX) throw runtime_error("GPU timeout or device removed");
}
int wmain(int argc,wchar_t** argv) try {
    if(argc<4 || argc>7) throw runtime_error("usage: streaming-test pack digest-or-tile-manifest feedback.dxil [expected-tile-index [rgba8 [sample.dxil]] | --diagonal | --diagonal-memory | --observe]");
    const bool observe = argc == 5 && wstring(argv[4]) == L"--observe";
    const bool memorySource = argc >= 5 && wstring(argv[4]) == L"--diagonal-memory";
    const bool diagonal = memorySource || (argc >= 5 && wstring(argv[4]) == L"--diagonal");
    if (diagonal && argc != 5) throw runtime_error("diagonal mode uses two R8 tiles and a manifest");
    const bool rgba = argc >= 6;
    if (rgba && wstring(argv[5]) != L"rgba8") throw runtime_error("unknown texture format");
    const UINT tileSide = rgba ? 128 : 256;
    constexpr UINT regionSide = 8;
    const UINT side = 2 * tileSide, regions = (side + regionSide - 1) / regionSide;
    const DXGI_FORMAT format = rgba ? DXGI_FORMAT_R8G8B8A8_UNORM : DXGI_FORMAT_R8_UNORM;
    ComPtr<IDXGIFactory6> dx; OK(CreateDXGIFactory1(IID_PPV_ARGS(&dx)));
    ComPtr<IDXGIAdapter1> adapter; OK(dx->EnumAdapterByGpuPreference(0,DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,IID_PPV_ARGS(&adapter)));
    DXGI_ADAPTER_DESC1 ad{}; OK(adapter->GetDesc1(&ad));
    ComPtr<ID3D12Device8> device; OK(D3D12CreateDevice(adapter.Get(),D3D_FEATURE_LEVEL_12_0,IID_PPV_ARGS(&device)));
    D3D12_FEATURE_DATA_D3D12_OPTIONS opt{}; OK(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS,&opt,sizeof(opt)));
    D3D12_FEATURE_DATA_D3D12_OPTIONS7 opt7{}; OK(device->CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS7,&opt7,sizeof(opt7)));
    wcout<<ad.Description<<L" tiled tier "<<opt.TiledResourcesTier<<L" feedback tier "<<opt7.SamplerFeedbackTier<<endl;
    if(!opt7.SamplerFeedbackTier || opt.TiledResourcesTier < D3D12_TILED_RESOURCES_TIER_2) throw runtime_error("device lacks required capabilities");
    D3D12_COMMAND_QUEUE_DESC qd{}; ComPtr<ID3D12CommandQueue> graphics; OK(device->CreateCommandQueue(&qd,IID_PPV_ARGS(&graphics)));
    ComPtr<ID3D12CommandAllocator> alloc; OK(device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT,IID_PPV_ARGS(&alloc)));
    ComPtr<ID3D12GraphicsCommandList1> cmd; OK(device->CreateCommandList(0,D3D12_COMMAND_LIST_TYPE_DIRECT,alloc.Get(),nullptr,IID_PPV_ARGS(&cmd)));
    ComPtr<ID3D12Fence> fence; OK(device->CreateFence(0,D3D12_FENCE_FLAG_NONE,IID_PPV_ARGS(&fence)));
    auto submit=[&](UINT64 v){OK(cmd->Close());ID3D12CommandList* lists[]={cmd.Get()};graphics->ExecuteCommandLists(1,lists);OK(graphics->Signal(fence.Get(),v));waitFor(fence.Get(),v);};
    auto reset=[&](){OK(alloc->Reset());OK(cmd->Reset(alloc.Get(),nullptr));};

    D3D12_RESOURCE_DESC td{};td.Dimension=D3D12_RESOURCE_DIMENSION_TEXTURE2D;td.Width=td.Height=side;td.DepthOrArraySize=1;td.MipLevels=1;td.Format=format;td.SampleDesc.Count=1;td.Layout=D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    ComPtr<ID3D12Resource> texture;OK(device->CreateReservedResource(&td,D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,nullptr,IID_PPV_ARGS(&texture)));
    D3D12_TILE_SHAPE tile{};UINT tiles=0;device->GetResourceTiling(texture.Get(),&tiles,nullptr,&tile,nullptr,0,nullptr);
    if(tile.WidthInTexels!=tileSide||tile.HeightInTexels!=tileSide||tiles!=4) throw runtime_error("unexpected test texture tiling");
    auto local=props(D3D12_HEAP_TYPE_DEFAULT),read=props(D3D12_HEAP_TYPE_READBACK);
    D3D12_RESOURCE_DESC1 fd{};fd.Dimension=td.Dimension;fd.Width=fd.Height=side;fd.DepthOrArraySize=1;fd.MipLevels=1;fd.Format=DXGI_FORMAT_SAMPLER_FEEDBACK_MIN_MIP_OPAQUE;fd.SampleDesc.Count=1;fd.Flags=D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;fd.SamplerFeedbackMipRegion={regionSide,regionSide,1};
    ComPtr<ID3D12Resource> feedback;OK(device->CreateCommittedResource2(&local,D3D12_HEAP_FLAG_NONE,&fd,D3D12_RESOURCE_STATE_UNORDERED_ACCESS,nullptr,nullptr,IID_PPV_ARGS(&feedback)));
    D3D12_RESOURCE_DESC decodedDesc=td;decodedDesc.Width=decodedDesc.Height=regions;decodedDesc.Format=DXGI_FORMAT_R8_UINT;decodedDesc.Layout=D3D12_TEXTURE_LAYOUT_UNKNOWN;
    ComPtr<ID3D12Resource> decoded;OK(device->CreateCommittedResource(&local,D3D12_HEAP_FLAG_NONE,&decodedDesc,D3D12_RESOURCE_STATE_RESOLVE_DEST,nullptr,IID_PPV_ARGS(&decoded)));
    D3D12_RESOURCE_DESC buf{};buf.Dimension=D3D12_RESOURCE_DIMENSION_BUFFER;buf.Width=65536;buf.Height=1;buf.DepthOrArraySize=1;buf.MipLevels=1;buf.SampleDesc.Count=1;buf.Layout=D3D12_TEXTURE_LAYOUT_ROW_MAJOR;
    auto readbackDesc = buf;
    if (diagonal) readbackDesc.Width *= 2;
    ComPtr<ID3D12Resource> rb;OK(device->CreateCommittedResource(&read,D3D12_HEAP_FLAG_NONE,&readbackDesc,D3D12_RESOURCE_STATE_COPY_DEST,nullptr,IID_PPV_ARGS(&rb)));
    D3D12_DESCRIPTOR_HEAP_DESC hd{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,4,D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,0};
    ComPtr<ID3D12DescriptorHeap> heap,cpu;OK(device->CreateDescriptorHeap(&hd,IID_PPV_ARGS(&heap)));hd.Flags=D3D12_DESCRIPTOR_HEAP_FLAG_NONE;OK(device->CreateDescriptorHeap(&hd,IID_PPV_ARGS(&cpu)));
    auto h=heap->GetCPUDescriptorHandleForHeapStart();device->CreateShaderResourceView(texture.Get(),nullptr,h);
    UINT stride=device->GetDescriptorHandleIncrementSize(hd.Type);h.ptr+=stride;device->CreateSamplerFeedbackUnorderedAccessView(texture.Get(),feedback.Get(),h);
    auto clear=cpu->GetCPUDescriptorHandleForHeapStart();device->CreateSamplerFeedbackUnorderedAccessView(texture.Get(),feedback.Get(),clear);
    D3D12_DESCRIPTOR_RANGE ranges[2]={{D3D12_DESCRIPTOR_RANGE_TYPE_SRV,1,0,0,0},{D3D12_DESCRIPTOR_RANGE_TYPE_UAV,1,0,0,1}};
    D3D12_ROOT_PARAMETER params[2]{};
    params[0].ParameterType=D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    params[0].DescriptorTable={2,ranges};
    params[1].ParameterType=D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS;
    params[1].Constants={0,0,2};
    D3D12_STATIC_SAMPLER_DESC samp{};samp.Filter=D3D12_FILTER_MIN_MAG_MIP_POINT;samp.AddressU=samp.AddressV=samp.AddressW=D3D12_TEXTURE_ADDRESS_MODE_CLAMP;samp.ComparisonFunc=D3D12_COMPARISON_FUNC_ALWAYS;samp.MaxAnisotropy=1;samp.MaxLOD=D3D12_FLOAT32_MAX;
    D3D12_ROOT_SIGNATURE_DESC rs{2,params,1,&samp,D3D12_ROOT_SIGNATURE_FLAG_NONE};ComPtr<ID3DBlob> blob,err;OK(D3D12SerializeRootSignature(&rs,D3D_ROOT_SIGNATURE_VERSION_1,&blob,&err));
    ComPtr<ID3D12RootSignature> root;OK(device->CreateRootSignature(0,blob->GetBufferPointer(),blob->GetBufferSize(),IID_PPV_ARGS(&root)));
    ifstream sf(filesystem::path(argv[3]),ios::binary);vector<char> code((istreambuf_iterator<char>(sf)),{});if(code.empty()) throw runtime_error("missing shader");
    D3D12_COMPUTE_PIPELINE_STATE_DESC ps{};ps.pRootSignature=root.Get();ps.CS={code.data(),code.size()};ComPtr<ID3D12PipelineState> pso;OK(device->CreateComputePipelineState(&ps,IID_PPV_ARGS(&pso)));
    auto gpu=heap->GetGPUDescriptorHandleForHeapStart(),uav=gpu;uav.ptr+=stride;ID3D12DescriptorHeap* heaps[]={heap.Get()};cmd->SetDescriptorHeaps(1,heaps);
    UINT init[]={255,255,255,255};cmd->ClearUnorderedAccessViewUint(uav,clear,feedback.Get(),init,0,nullptr);
    D3D12_RESOURCE_BARRIER order{};order.Type=D3D12_RESOURCE_BARRIER_TYPE_UAV;order.UAV.pResource=feedback.Get();cmd->ResourceBarrier(1,&order);
    cmd->SetComputeRootSignature(root.Get());cmd->SetPipelineState(pso.Get());cmd->SetComputeRootDescriptorTable(0,gpu);cmd->Dispatch(1,1,1);
    transition(cmd.Get(),feedback.Get(),D3D12_RESOURCE_STATE_UNORDERED_ACCESS,D3D12_RESOURCE_STATE_RESOLVE_SOURCE);
    cmd->ResolveSubresourceRegion(decoded.Get(),0,0,0,feedback.Get(),UINT_MAX,nullptr,DXGI_FORMAT_R8_UINT,D3D12_RESOLVE_MODE_DECODE_SAMPLER_FEEDBACK);
    transition(cmd.Get(),decoded.Get(),D3D12_RESOURCE_STATE_RESOLVE_DEST,D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
    transition(cmd.Get(),texture.Get(),D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,D3D12_RESOURCE_STATE_COMMON);
    auto maskDesc=buf; maskDesc.Width=sizeof(UINT); maskDesc.Flags=D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    ComPtr<ID3D12Resource> mask;
    OK(device->CreateCommittedResource(&local,D3D12_HEAP_FLAG_NONE,&maskDesc,D3D12_RESOURCE_STATE_UNORDERED_ACCESS,nullptr,IID_PPV_ARGS(&mask)));
    auto compactCpu=h; compactCpu.ptr+=stride;
    device->CreateShaderResourceView(decoded.Get(),nullptr,compactCpu);
    compactCpu.ptr+=stride;
    D3D12_UNORDERED_ACCESS_VIEW_DESC maskView{};
    maskView.ViewDimension=D3D12_UAV_DIMENSION_BUFFER;maskView.Buffer.NumElements=1;maskView.Buffer.StructureByteStride=4;
    device->CreateUnorderedAccessView(mask.Get(),nullptr,&maskView,compactCpu);
    ifstream compactFile(filesystem::path(argv[0]).parent_path()/L"compact-feedback.dxil",ios::binary);
    vector<char> compactCode((istreambuf_iterator<char>(compactFile)),{});
    if(compactCode.empty())throw runtime_error("compact-feedback.dxil missing beside streaming-test");
    auto compactDesc=ps;compactDesc.CS={compactCode.data(),compactCode.size()};
    ComPtr<ID3D12PipelineState> compactPipeline;OK(device->CreateComputePipelineState(&compactDesc,IID_PPV_ARGS(&compactPipeline)));
    auto compactGpu=gpu;compactGpu.ptr+=2*stride;
    cmd->SetPipelineState(compactPipeline.Get());cmd->SetComputeRootDescriptorTable(0,compactGpu);
    const UINT regionsPerTile[]={tile.WidthInTexels/regionSide,tile.HeightInTexels/regionSide};
    cmd->SetComputeRoot32BitConstants(1,2,regionsPerTile,0);cmd->Dispatch(1,1,1);
    transition(cmd.Get(),mask.Get(),D3D12_RESOURCE_STATE_UNORDERED_ACCESS,D3D12_RESOURCE_STATE_COPY_SOURCE);
    cmd->CopyBufferRegion(rb.Get(),0,mask.Get(),0,sizeof(UINT));submit(1);
    void* mapped=nullptr;D3D12_RANGE range{0,sizeof(UINT)},empty{};OK(rb->Map(0,&range,&mapped));
    const UINT bits=*static_cast<const UINT*>(mapped);
    rb->Unmap(0,&empty);
    UINT expectedMask = 0b1001;
    if (observe) {
        if (bits != 2) throw runtime_error("observation did not select the expected tile");
        cout << "Observation only: " << regionSide << "x" << regionSide
             << " regions, " << regions << "x" << regions
             << " resolve, 4 bytes read back, mask " << bits << endl;
        return 0;
    }
    if (!diagonal) {
        const UINT wanted = argc >= 5 ? stoul(argv[4]) : 1;
        if (wanted >= tiles) throw runtime_error("expected tile outside test texture");
        expectedMask = 1u << wanted;
    }
    if (bits != expectedMask) throw runtime_error("unexpected GPU feedback mask: " + to_string(bits));
    cout << "GPU feedback mask " << bits << " (4 bytes read back)" << endl;

    const UINT count = std::popcount(bits);
    const UINT64 packLength = filesystem::file_size(argv[1]);
    struct Block { UINT64 offset, bytes; string expected; };
    vector<Block> blocks;
    vector<D3D12_TILED_RESOURCE_COORDINATE> coords;
    ifstream manifest;
    if (argc >= 5) manifest.open(filesystem::path(argv[2]));
    for (UINT i = 0; i < std::bit_width(bits); ++i) {
        Block b{};
        if (argc >= 5) {
            if (!(manifest >> b.offset >> b.bytes >> b.expected)) throw runtime_error("incomplete tile manifest");
        } else if (bits & (1u << i)) {
            b = {0, packLength, filesystem::path(argv[2]).string()};
        }
        if (!(bits & (1u << i))) continue;
        if (!b.bytes || b.bytes > UINT32_MAX || b.offset > packLength || b.bytes > packLength - b.offset)
            throw runtime_error("tile stream lies outside pack");
        coords.push_back({i % 2, i / 2, 0, 0});
        blocks.push_back(b);
    }
    if (diagonal && blocks[0].expected == blocks[1].expected)
        throw runtime_error("diagonal test needs distinct tile content");
    D3D12_HEAP_DESC physical{};physical.SizeInBytes=count*65536;physical.Properties=local;physical.Flags=D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES;
    ComPtr<ID3D12Heap> tileHeap;OK(device->CreateHeap(&physical,IID_PPV_ARGS(&tileHeap)));
    vector<D3D12_TILE_REGION_SIZE> regionsToMap(count, {1,FALSE,0,0,0});
    vector<D3D12_TILE_RANGE_FLAGS> flags(count,D3D12_TILE_RANGE_FLAG_NONE);
    vector<UINT> offsets(count), counts(count, 1);
    for (UINT i = 0; i < count; ++i) offsets[i] = count - i - 1;
    graphics->UpdateTileMappings(texture.Get(),count,coords.data(),regionsToMap.data(),tileHeap.Get(),
                                 count,flags.data(),offsets.data(),counts.data(),D3D12_TILE_MAPPING_FLAG_NONE);
    OK(graphics->Signal(fence.Get(),2));
    ComPtr<IDStorageFactory> ds;OK(DStorageGetFactory(IID_PPV_ARGS(&ds)));
    // Keep the compressed source alive for the lifetime of the storage queue.
    vector<vector<char>> compressed;
    ComPtr<IDStorageFile> file;
    if (memorySource) {
        constexpr UINT64 maxCompressedTile = 2 * 65536;
        ifstream pack(filesystem::path(argv[1]), ios::binary);
        compressed.resize(count);
        for (UINT i = 0; i < count; ++i) {
            if (blocks[i].bytes > maxCompressedTile) throw runtime_error("oversized compressed tile");
            auto& source = compressed[i];
            source.resize(static_cast<size_t>(blocks[i].bytes));
            pack.seekg(static_cast<streamoff>(blocks[i].offset));
            if (!pack.read(source.data(), static_cast<streamsize>(source.size())))
                throw runtime_error("could not read compressed tile into system memory");
        }
    } else {
        OK(ds->OpenFile(argv[1],IID_PPV_ARGS(&file)));
    }
    DSTORAGE_QUEUE_DESC q{};
    q.Capacity = DSTORAGE_MIN_QUEUE_CAPACITY;
    q.SourceType = memorySource ? DSTORAGE_REQUEST_SOURCE_MEMORY : DSTORAGE_REQUEST_SOURCE_FILE;
    q.Device = device.Get();
    q.Name = "fx1 verified texture tile";
    ComPtr<IDStorageQueue3> storage;OK(ds->CreateQueue(&q,IID_PPV_ARGS(&storage)));auto support=storage->GetCompressionSupport(DSTORAGE_COMPRESSION_FORMAT_GDEFLATE);
    cout<<"GDeflate compression support 0x"<<hex<<support<<dec<<endl;
    if(!(support & (DSTORAGE_COMPRESSION_SUPPORT_GPU_OPTIMIZED|DSTORAGE_COMPRESSION_SUPPORT_GPU_FALLBACK)) || (support & DSTORAGE_COMPRESSION_SUPPORT_CPU_FALLBACK))throw runtime_error("queue uses CPU decompression");
    vector<DSTORAGE_REQUEST> requests(count);
    for (UINT i = 0; i < count; ++i) {
        auto& r = requests[i];
        r.Options.SourceType=q.SourceType;r.Options.DestinationType=DSTORAGE_REQUEST_DESTINATION_TILES;r.Options.CompressionFormat=DSTORAGE_COMPRESSION_FORMAT_GDEFLATE;
        if (memorySource)
            r.Source.Memory = {compressed[i].data(), static_cast<UINT>(compressed[i].size())};
        else
            r.Source.File = {file.Get(), blocks[i].offset, static_cast<UINT>(blocks[i].bytes)};
        r.UncompressedSize=65536;r.Destination.Tiles={texture.Get(),coords[i],regionsToMap[i]};
    }
    // The storage queue waits for mapping before GPU work. Graphics then
    // waits for the storage fence before consuming either tile.
    storage->EnqueueRequests(requests.data(),count,fence.Get(),2,DSTORAGE_ENQUEUE_REQUEST_FLAG_FENCE_WAIT_BEFORE_GPU_WORK);
    storage->EnqueueSignal(fence.Get(),3);
    storage->Submit();
    OK(graphics->Wait(fence.Get(),3));
    range.End=65536;
    if (argc == 7) {
        const auto tx=coords.front().X, ty=coords.front().Y;
        ifstream sampleFile(filesystem::path(argv[6]), ios::binary);
        vector<char> sampleCode((istreambuf_iterator<char>(sampleFile)), {});
        if (sampleCode.empty()) throw runtime_error("sample shader missing");
        ps.CS = {sampleCode.data(), sampleCode.size()};
        ComPtr<ID3D12PipelineState> samplePipeline;
        OK(device->CreateComputePipelineState(&ps, IID_PPV_ARGS(&samplePipeline)));
        auto outputDesc = buf;
        outputDesc.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        ComPtr<ID3D12Resource> output;
        OK(device->CreateCommittedResource(&local, D3D12_HEAP_FLAG_NONE, &outputDesc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS, nullptr, IID_PPV_ARGS(&output)));
        D3D12_UNORDERED_ACCESS_VIEW_DESC view{};
        view.ViewDimension = D3D12_UAV_DIMENSION_BUFFER;
        view.Buffer.StructureByteStride = 4;
        view.Buffer.NumElements = tileSide * tileSide;
        device->CreateUnorderedAccessView(output.Get(), nullptr, &view, h);
        reset();
        transition(cmd.Get(),texture.Get(),D3D12_RESOURCE_STATE_COMMON,D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
        cmd->SetDescriptorHeaps(1, heaps);
        cmd->SetComputeRootSignature(root.Get());
        cmd->SetComputeRootDescriptorTable(0, gpu);
        const UINT origin[] = {tx * tileSide, ty * tileSide};
        cmd->SetComputeRoot32BitConstants(1, 2, origin, 0);
        cmd->SetPipelineState(samplePipeline.Get());
        cmd->Dispatch(tileSide / 8, tileSide / 8, 1);
        transition(cmd.Get(), output.Get(), D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        cmd->CopyBufferRegion(rb.Get(), 0, output.Get(), 0, outputDesc.Width);
        submit(5);
        OK(rb->Map(0, &range, &mapped));
        auto sampledHash = digest(static_cast<BYTE*>(mapped), 65536);
        rb->Unmap(0, &empty);
        if (sampledHash != blocks.front().expected) throw runtime_error("sampled RGBA digest mismatch: " + sampledHash);
        cout << "Sampler returned all 16384 RGBA texels, SHA256 " << sampledHash << endl;
    } else {
        reset();transition(cmd.Get(),texture.Get(),D3D12_RESOURCE_STATE_COMMON,D3D12_RESOURCE_STATE_COPY_SOURCE);
        D3D12_TEXTURE_COPY_LOCATION dst{};dst.pResource=rb.Get();dst.Type=D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT;
        dst.PlacedFootprint.Footprint={format,tileSide,tileSide,1,tileSide * (rgba ? 4u : 1u)};
        D3D12_TEXTURE_COPY_LOCATION src{};src.pResource=texture.Get();
        for (UINT i = 0; i < count; ++i) {
            const auto tx=coords[i].X, ty=coords[i].Y;
            D3D12_BOX box{tx*tileSide,ty*tileSide,0,(tx+1)*tileSide,(ty+1)*tileSide,1};
            dst.PlacedFootprint.Offset=i*65536;
            cmd->CopyTextureRegion(&dst,0,0,0,&src,&box);
        }
        submit(4);
        range.End=count*65536;
        OK(rb->Map(0,&range,&mapped));
        vector<string> hashes;
        for (UINT i = 0; i < count; ++i) hashes.push_back(digest(static_cast<BYTE*>(mapped)+i*65536,65536));
        rb->Unmap(0,&empty);
        for (UINT i = 0; i < count; ++i) {
            if (hashes[i] != blocks[i].expected) throw runtime_error("tile digest mismatch: "+hashes[i]);
            cout<<"GPU tile ("<<coords[i].X<<","<<coords[i].Y<<") verified 65536 bytes SHA256 "<<hashes[i]<<endl;
        }
    }
    DSTORAGE_ERROR_RECORD errors{};storage->RetrieveErrorRecord(&errors);
    if(errors.FailureCount)OK(errors.FirstFailure.HResult);
    if (memorySource) cout << "Compressed CPU memory to device-local tiles verified" << endl;
    return 0;
} catch(const exception& e) {cerr<<e.what()<<endl;return 1;}
