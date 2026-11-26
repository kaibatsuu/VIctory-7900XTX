// Process-local private CSV. File output and timestamps are disabled by default.
#pragma once
#include <fcntl.h>
#include <io.h>
#include <share.h>
#include <sys/stat.h>
#include <cwchar>

struct MarkerSource {
    uint64_t source_id, name_id, file_offset;
    uint32_t file, reserved;
};
static_assert(sizeof(MarkerSource) == 32);

struct MarkerRequest {
    uint64_t number{};
    MarkerSource source{};
};

struct MarkerEvent {
    MarkerRequest request{};
    const Record* record{};
    UINT tile{};
};

class AssetMarkers {
    std::mutex writer;
    std::FILE* file = nullptr;
    std::atomic<bool> active{false};
    uint64_t sequence = 0;
    uint64_t limit = 65536;
    uint64_t last_flush = 0;
public:
    void open() noexcept {
        const wchar_t* path = _wgetenv(L"VICTORY_GDEFLATE_MARKERS");
        if (!path || !*path) return;
        int fd = -1;
        if (_wsopen_s(&fd, path, _O_CREAT | _O_EXCL | _O_WRONLY | _O_BINARY,
                      _SH_DENYWR, _S_IREAD | _S_IWRITE) ||
            !(file = _wfdopen(fd, L"wb"))) {
            if (fd >= 0) _close(fd);
            std::fputs("gdeflate: cannot create new private marker CSV\n", stderr);
            return;
        }
        if (const wchar_t* value = _wgetenv(L"VICTORY_GDEFLATE_MARKER_LIMIT")) {
            wchar_t* end = nullptr;
            const auto count = std::wcstoull(value, &end, 10);
            if (*value && !*end && count >= 1 && count <= 1000000) limit = count;
        }
        std::fputs("seq,utc_us,request,event,source_kind,source_id,name_id,file_offset,"
                   "stored_bytes,pack_offset,compressed_bytes,decoded_bytes,tile,asset_id\n", file);
        std::fflush(file);
        active.store(true, std::memory_order_release);
    }

    bool enabled() const noexcept { return active.load(std::memory_order_acquire); }

    void write(const char* event, const MarkerEvent& m) noexcept {
        if (!enabled()) return;
        std::lock_guard lock(writer);
        if (!file) return;
        FILETIME time{};
        GetSystemTimePreciseAsFileTime(&time);
        ULARGE_INTEGER ticks{};
        ticks.LowPart = time.dwLowDateTime;
        ticks.HighPart = time.dwHighDateTime;
        const uint64_t utc_us = (ticks.QuadPart - 116444736000000000ULL) / 10;
        const auto seq = ++sequence;
        if (seq > limit) {
            std::fprintf(file, "%llu,%llu,,limit,,,,,,,,,,\n", seq, utc_us);
            close();
            return;
        }
        const auto& src = m.request.source;
        const auto& r = *m.record;
        char offset[24] = "";
        if (src.file == 1) std::snprintf(offset, sizeof(offset), "%llu", src.file_offset);
        // Prefix of the compressed-source SHA-256 in the index, in digest order.
        char asset[17];
        static constexpr char hex[] = "0123456789abcdef";
        for (size_t i = 0; i < 8; ++i) {
            asset[i * 2] = hex[r.source[i] >> 4];
            asset[i * 2 + 1] = hex[r.source[i] & 15];
        }
        asset[16] = 0;
        const int bytes = std::fprintf(file, "%llu,%llu,%llu,%s,%s,%016llx,%016llx,%s,"
                                       "%u,%llu,%u,%u,%u,%s\n", seq, utc_us, m.request.number,
                                       event, src.file == 1 ? "file" : src.file == 0 ? "memory" : "unknown", src.source_id,
                                       src.name_id, offset, r.stored, r.offset, r.compressed,
                                       r.decoded, m.tile, asset);
        if (bytes < 0) { close(); return; }
        // Amortize writes, retaining the first event and regularly flushing for live readers.
        if (seq % 128 == 0 || utc_us - last_flush >= 250000) {
            if (std::fflush(file)) close();
            last_flush = utc_us;
        }
    }
private:
    void close() noexcept {
        active.store(false, std::memory_order_release);
        std::fclose(file);
        file = nullptr;
    }
};

static AssetMarkers markers;
// Paired read/finish runs on one storage worker. Index entries live for the process.
static thread_local MarkerEvent awaiting_destination;
