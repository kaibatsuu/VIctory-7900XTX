#pragma once
#include <thread>

// Only descriptors cross the thread boundary. Their lane leases keep both
// resources live and exclusive until their calling worker delivers the result.
struct SingleSubmitter {
    using Clock = std::chrono::steady_clock;
    static constexpr UINT max_slots = 4;
    struct Job {
        DSTORAGE_REQUEST request{};
        ID3D12Resource* readback = nullptr;
        UINT slot = 0, offset = 0, bytes = 0;
        Clock::time_point arrival;
        HRESULT result = E_PENDING;
        bool finished = false;
    };
    enum Metric { Jobs, Groups, Bytes, LargestGroup, LargestBytes, CpuFenceWaits,
        OversizeGroups, DeadlineGroups, ByteGroups, SlotGroups, WorkerStarts,
        OwnerTid, PeakPending, MaxAgeUs, FailedGroups, Count };
    static inline std::array<std::atomic<uint64_t>, Count> totals{};
    static void peak(Metric metric, uint64_t value) {
        auto& counter = totals[metric];
        auto prior = counter.load(std::memory_order_relaxed);
        while (prior < value && !counter.compare_exchange_weak(prior, value, std::memory_order_relaxed)) {}
    }
    IDStorageQueue2* storage = nullptr;
    ComPtr<IDStorageQueue3> batch_storage;
    IDStorageStatusArray* status = nullptr;
    ComPtr<ID3D12CommandQueue> copy;
    ComPtr<ID3D12CommandAllocator> allocator;
    ComPtr<ID3D12GraphicsCommandList> commands;
    ComPtr<ID3D12Fence> fence;
    UINT64 sequence = 0;
    UINT slots = 0, ttl_us = 0, budget = 0;
    HANDLE changed = nullptr, timer = nullptr, completed = nullptr;
    std::array<Job*, max_slots> pending{};
    UINT queued = 0;
    std::mutex gate;
    std::condition_variable done;
    std::thread worker;
    bool stopping = false;

    ~SingleSubmitter() {
        stop();
        if (changed) CloseHandle(changed);
        if (timer) CloseHandle(timer);
        if (completed) CloseHandle(completed);
    }
    void start(ID3D12Device* dev, IDStorageQueue2* queue, IDStorageStatusArray* statuses, UINT slot_count) {
        storage = queue;
        queue->QueryInterface(IID_PPV_ARGS(&batch_storage));
        status = statuses;
        slots = slot_count;
        ttl_us = SubmissionBatch::setting(L"VICTORY_GDEFLATE_BATCH_US", 250, 2000);
        budget = SubmissionBatch::setting(L"VICTORY_GDEFLATE_BATCH_KIB", 512, 4096) * 1024;
        if (!budget) budget = 512 * 1024;
        D3D12_COMMAND_QUEUE_DESC desc{};
        desc.Type = D3D12_COMMAND_LIST_TYPE_COPY;
        checked(dev->CreateCommandQueue(&desc, IID_PPV_ARGS(&copy)));
        checked(dev->CreateCommandAllocator(desc.Type, IID_PPV_ARGS(&allocator)));
        checked(dev->CreateCommandList(0, desc.Type, allocator.Get(), nullptr, IID_PPV_ARGS(&commands)));
        checked(commands->Close());
        checked(dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)));
        changed = CreateEventW(nullptr, FALSE, FALSE, nullptr);
        completed = CreateEventW(nullptr, FALSE, FALSE, nullptr);
        timer = CreateWaitableTimerExW(nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS);
        if (!changed || !completed || !timer) throw E_FAIL;
        worker = std::thread([this] { run(); });
    }
    // Shutdown is serialized by the bridge initialization lock. A group already
    // taken by the worker runs to completion; pending groups drain without delay.
    void stop() {
        { std::lock_guard lock(gate); stopping = true; }
        if (changed) SetEvent(changed);
        if (worker.joinable()) worker.join();
    }
    void stage(Job& job) {
        std::unique_lock lock(gate);
        if (stopping || queued >= slots) throw E_ABORT;
        job.arrival = Clock::now();
        pending[queued++] = &job;
        peak(PeakPending, queued);
        SetEvent(changed);
        done.wait(lock, [&] { return job.finished; });
        checked(job.result);
    }
    void execute(Job** jobs, UINT count) {
        checked(allocator->Reset());
        checked(commands->Reset(allocator.Get(), nullptr));
        // Every lane keeps its own readback and completion. Identical
        // immutable pack ranges share one decompression within this group.
        std::array<DSTORAGE_REQUEST, max_slots> requests{};
        std::array<Job*, max_slots> owners{};
        std::array<Job*, max_slots> source{};
        UINT unique = 0;
        for (UINT i = 0; i < count; ++i) {
            const auto& job = *jobs[i];
            const auto& block = job.request.Source.File;
            UINT entry = 0;
            for (; entry < unique; ++entry) {
                const auto& candidate = requests[entry].Source.File;
                if (candidate.Source == block.Source && candidate.Offset == block.Offset &&
                    candidate.Size == block.Size && requests[entry].UncompressedSize == job.bytes)
                    break;
            }
            if (entry == unique) {
                requests[unique] = job.request;
                owners[unique++] = jobs[i];
            }
            source[i] = owners[entry];
            const auto& producer = *source[i];
            commands->CopyBufferRegion(job.readback, job.offset,
                                       producer.request.Destination.Buffer.Resource,
                                       producer.offset, job.bytes);
        }
        checked(commands->Close());
        if (batch_storage) {
            // All ranges are from the same pack. Keep the scheduler's group
            // membership, but issue its independent I/O in file order.
            std::stable_sort(requests.begin(), requests.begin() + unique,
                             [](const auto& a, const auto& b) {
                                 return a.Source.File.Offset < b.Source.File.Offset;
                             });
            batch_storage->EnqueueRequests(requests.data(), unique, nullptr, 0,
                                           DSTORAGE_ENQUEUE_REQUEST_FLAG_NONE);
            storage->EnqueueStatus(status, jobs[0]->slot);
        } else {
            for (UINT i = 0; i < unique; ++i) {
                storage->EnqueueRequest(&requests[i]);
                storage->EnqueueStatus(status, owners[i]->slot);
            }
        }
        storage->EnqueueSignal(fence.Get(), ++sequence);
        storage->Submit();
        checked(copy->Wait(fence.Get(), sequence));
        ID3D12CommandList* lists[] = {commands.Get()};
        copy->ExecuteCommandLists(1, lists);
        checked(copy->Signal(fence.Get(), ++sequence));
        checked(fence->SetEventOnCompletion(sequence, completed));
        ++totals[CpuFenceWaits];
        if (WaitForSingleObject(completed, 10000) != WAIT_OBJECT_0 || fence->GetCompletedValue() == UINT64_MAX)
            throw DXGI_ERROR_DEVICE_HUNG;
        for (UINT i = 0; i < count; ++i) jobs[i]->result = status->GetHResult(batch_storage ? jobs[0]->slot : source[i]->slot);
    }
    void sample() const {
        const auto groups = totals[Groups].load();
        if (groups != 1 && groups % 128 != 0) return;
        std::fprintf(stderr, "gdeflate single_submit slots=%u ttl_us=%u budget_bytes=%u owner_tid=%llu workers=%llu jobs=%llu groups=%llu bytes=%llu peak_group=%llu peak_bytes=%llu fence_waits=%llu oversized=%llu deadline=%llu byte_cap=%llu slot_cap=%llu max_age_us=%llu errors=%llu\n",
            slots, ttl_us, budget, totals[OwnerTid].load(), totals[WorkerStarts].load(), totals[Jobs].load(),
            groups, totals[Bytes].load(), totals[LargestGroup].load(), totals[LargestBytes].load(),
            totals[CpuFenceWaits].load(), totals[OversizeGroups].load(), totals[DeadlineGroups].load(),
            totals[ByteGroups].load(), totals[SlotGroups].load(), totals[MaxAgeUs].load(), totals[FailedGroups].load());
    }
    void run() {
        ++totals[WorkerStarts];
        totals[OwnerTid] = GetCurrentThreadId();
        std::unique_lock lock(gate);
        for (;;) {
            if (!queued) {
                if (stopping) return;
                lock.unlock();
                WaitForSingleObject(changed, INFINITE);
                lock.lock();
                continue;
            }
            UINT count = 1, bytes = pending[0]->bytes;
            while (count < queued && bytes < budget && pending[count]->bytes <= budget - bytes)
                bytes += pending[count++]->bytes;
            const auto now = Clock::now();
            const auto deadline = pending[0]->arrival + std::chrono::microseconds(ttl_us);
            if (!stopping && count == queued && count < slots && bytes < budget && now < deadline) {
                LARGE_INTEGER due{};
                due.QuadPart = -std::max<int64_t>(1, std::chrono::duration_cast<std::chrono::nanoseconds>(deadline - now).count() / 100);
                if (SetWaitableTimer(timer, &due, 0, nullptr, nullptr, FALSE)) {
                    HANDLE events[] = {changed, timer};
                    lock.unlock();
                    const auto wake = WaitForMultipleObjects(2, events, FALSE, INFINITE);
                    lock.lock();
                    if (wake == WAIT_OBJECT_0 || wake == WAIT_OBJECT_0 + 1) continue;
                }
            }
            std::array<Job*, max_slots> group;
            std::copy_n(pending.begin(), count, group.begin());
            for (UINT i = count; i < queued; ++i) pending[i - count] = pending[i];
            queued -= count;
            ++totals[Groups];
            totals[Jobs] += count;
            totals[Bytes] += bytes;
            peak(LargestGroup, count);
            peak(LargestBytes, bytes);
            peak(MaxAgeUs, std::chrono::duration_cast<std::chrono::microseconds>(Clock::now() - group[0]->arrival).count());
            if (bytes > budget) ++totals[OversizeGroups];
            else if (bytes == budget || queued) ++totals[ByteGroups];
            else if (count == slots) ++totals[SlotGroups];
            else ++totals[DeadlineGroups];
            lock.unlock();
            HRESULT error = S_OK;
            try { execute(group.data(), count); }
            catch (HRESULT hr) { error = hr; }
            catch (...) { error = E_FAIL; }
            sample();
            lock.lock();
            for (UINT i = 0; i < count; ++i) {
                if (FAILED(error)) group[i]->result = error;
                group[i]->finished = true;
            }
            if (FAILED(error)) {
                ++totals[FailedGroups];
                stopping = true;
                for (UINT i = 0; i < queued; ++i) {
                    pending[i]->result = error;
                    pending[i]->finished = true;
                }
                queued = 0;
            }
            done.notify_all();
        }
    }
};
