// 独立原生研究工具，不调用 Cargo，也不把本机动态 zstd 结果冒充 Rust source 结果。
// 编译时显式指定 zstd 头文件和库；输出真实流式解码的分配请求，不以 RSS 猜测分配。
#define ZSTD_STATIC_LINKING_ONLY
#include <zstd.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct AllocationCounts { size_t calls; size_t maximum; size_t total; };
static void *counted_allocate(void *opaque, size_t bytes) {
    struct AllocationCounts *counts = opaque;
    counts->calls++;
    counts->total += bytes;
    if (bytes > counts->maximum) counts->maximum = bytes;
    return malloc(bytes);
}
static void counted_free(void *opaque, void *allocation) {
    (void)opaque;
    free(allocation);
}

int main(int argc, char **argv) {
    unsigned window_log = argc > 1 ? (unsigned)strtoul(argv[1], NULL, 10) : 10;
    if (window_log < 10 || window_log > 27) return 2;
    const char payload[] = "{\"type\":\"session\",\"id\":\"<child-id>\",\"origin\":\"subagent\",\"parentSession\":\"session-<parent-id>\"}\n";
    size_t payload_size = strlen(payload);
    unsigned char frame[1024];
    size_t length = 0;
    const unsigned char magic[] = {0x28, 0xb5, 0x2f, 0xfd};
    memcpy(frame, magic, sizeof(magic)); length += sizeof(magic);
    frame[length++] = 0; // unknown content size, non-single-segment, no checksum/dictionary
    frame[length++] = (unsigned char)((window_log - 10) << 3);
    uint32_t block = (uint32_t)(payload_size << 3); // non-final raw block
    frame[length++] = (unsigned char)block;
    frame[length++] = (unsigned char)(block >> 8);
    frame[length++] = (unsigned char)(block >> 16);
    memcpy(frame + length, payload, payload_size); length += payload_size;
    frame[length++] = 1; frame[length++] = 0; frame[length++] = 0; // final empty raw block

    ZSTD_frameHeader header;
    size_t parsed = ZSTD_getFrameHeader(&header, frame, length);
    if (parsed != 0 || ZSTD_isError(parsed)) return 3;
    struct AllocationCounts counts = {0};
    ZSTD_customMem memory = {counted_allocate, counted_free, &counts};
    ZSTD_DStream *stream = ZSTD_createDStream_advanced(memory);
    if (!stream) return 4;
    size_t initialized = ZSTD_initDStream(stream);
    if (ZSTD_isError(initialized)) { ZSTD_freeDStream(stream); return 5; }
    char output_bytes[65537];
    ZSTD_inBuffer input = {frame, length, 0};
    ZSTD_outBuffer output = {output_bytes, sizeof(output_bytes), 0};
    size_t result = ZSTD_decompressStream(stream, &output, &input);
    int valid = !ZSTD_isError(result) && result == 0 && output.pos == payload_size &&
        memcmp(output_bytes, payload, payload_size) == 0;
    printf("zstd=%s window_log=%u window_bytes=%llu input_bytes=%zu consumed=%zu decoded_bytes=%zu allocations=%zu max_requested=%zu total_requested=%zu valid=%d\n",
        ZSTD_versionString(), window_log, (unsigned long long)header.windowSize,
        length, input.pos, output.pos, counts.calls, counts.maximum, counts.total, valid);
    if (ZSTD_isError(result)) fprintf(stderr, "%s\n", ZSTD_getErrorName(result));
    ZSTD_freeDStream(stream);
    return valid ? 0 : 6;
}
