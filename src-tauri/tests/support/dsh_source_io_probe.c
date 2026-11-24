// 仅供 macOS 隔离测试子进程加载：观测真实 I/O/分配，边界故障只修改测试目录。
// 不替换 source、解析结果、系统调用实参或分配返回值。
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
#include <stdint.h>
#include <stdarg.h>
#include <dirent.h>
#include <stdatomic.h>

static int swapped = 0;
static int target_read = 0;
static int header_prechecked = 0;
static int header_opened = 0;
static dev_t header_dev;
static ino_t header_ino;
static int header_data_pending = 0;
static int parent_opened = 0;
static dev_t parent_dev;
static ino_t parent_ino;
static int parent_data_pending = 0;
static int header_openat_recorded = 0;
static int armed_child_snapshot = 0;
static int armed_parent_snapshot = 0;
static int parent_read = 0;
static int parent_postchecked = 0;
static int child_postchecked = 0;
static int parent_barrier_recorded = 0;
static int reread_phase = 0;
static int reread_parent_data = 0;
static int sessions_root_validated = 0;
static int root_barrier_recorded = 0;
static unsigned long long root_validated_inode = 0;
static atomic_int allocation_scope_active = 0;
static atomic_int allocation_recording = 0;

static void record(const char *kind, long long amount, const char *path) {
    const char *trace = getenv("H08_IO_TRACE");
    if (!trace) return;
    char line[PATH_MAX + 96];
    int length = snprintf(line, sizeof(line), "%s\t%lld\t%s\n", kind, amount, path);
    if (length <= 0 || (size_t)length >= sizeof(line)) return;
    int fd = open(trace, O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (fd < 0) return;
    // 探针日志不经 read，不递归进入观测回调。
    ssize_t ignored = write(fd, line, (size_t)length);
    (void)ignored;
    close(fd);
}

static void parent_reference(const char *path, const struct stat *metadata) {
    record("REREAD_REFERENCE_INODE", metadata->st_ino, path);
    record("REREAD_REFERENCE_SIZE", metadata->st_size, path);
    record("REREAD_REFERENCE_MTIME_SEC", metadata->st_mtimespec.tv_sec, path);
    record("REREAD_REFERENCE_MTIME_NSEC", metadata->st_mtimespec.tv_nsec, path);
    record("REREAD_REFERENCE_CTIME_SEC", metadata->st_ctimespec.tv_sec, path);
    record("REREAD_REFERENCE_CTIME_NSEC", metadata->st_ctimespec.tv_nsec, path);
}

static int rewrite_parent_in_place(const char *path, const char *from, const struct stat *reference) {
    char bytes[8192];
    struct stat staged;
    int input = from ? open(from, O_RDONLY) : -1;
    if (input < 0) return -1;
    if (fstat(input, &staged) || staged.st_size != reference->st_size || staged.st_size < 0 ||
        staged.st_size > (off_t)sizeof(bytes)) { close(input); errno = EINVAL; return -1; }
    size_t loaded = 0;
    while (loaded < (size_t)staged.st_size) {
        ssize_t amount = read(input, bytes + loaded, (size_t)staged.st_size - loaded);
        if (amount < 0 && errno == EINTR) continue;
        if (amount <= 0) { close(input); errno = EIO; return -1; }
        loaded += (size_t)amount;
    }
    close(input);
    int output = open(path, O_WRONLY | O_TRUNC);
    if (output < 0) return -1;
    size_t written = 0;
    while (written < loaded) {
        ssize_t amount = write(output, bytes + written, loaded - written);
        if (amount < 0 && errno == EINTR) continue;
        if (amount <= 0) { close(output); errno = EIO; return -1; }
        written += (size_t)amount;
    }
    struct timespec times[2] = {reference->st_atimespec, reference->st_mtimespec};
    int result = futimens(output, times);
    struct stat after;
    if (result == 0 && fstat(output, &after) == 0) {
        record("REREAD_AFTER_INODE", after.st_ino, path);
        record("REREAD_AFTER_SIZE", after.st_size, path);
        record("REREAD_AFTER_MTIME_SEC", after.st_mtimespec.tv_sec, path);
        record("REREAD_AFTER_MTIME_NSEC", after.st_mtimespec.tv_nsec, path);
        record("REREAD_AFTER_CTIME_SEC", after.st_ctimespec.tv_sec, path);
        record("REREAD_AFTER_CTIME_NSEC", after.st_ctimespec.tv_nsec, path);
    } else result = -1;
    close(output);
    return result;
}

static void reread_boundary(const char *mode, const char *boundary, const char *path, const char *target, const struct stat *snapshot) {
    const char *parent = getenv("H08_IO_PARENT_TARGET");
    if (!parent || reread_phase >= 2) return;
    if (reread_phase == 0) {
        if (!strcmp(boundary, "read")) {
            if (!strcmp(path, target)) target_read = 1;
            if (!strcmp(path, parent)) parent_read = 1;
        }
        if (!strcmp(boundary, "snapshot")) {
            if (!strcmp(path, target) && target_read) child_postchecked = 1;
            if (!strcmp(path, parent) && parent_read) parent_postchecked = 1;
        }
        if (child_postchecked && parent_postchecked) {
            const char bytes[] = "{\"type\":\"probe/body\"}\n";
            int fd = open(parent, O_WRONLY | O_APPEND);
            ssize_t written = fd < 0 ? -1 : write(fd, bytes, sizeof(bytes) - 1);
            if (fd >= 0) close(fd);
            reread_phase = 1;
            record(written == (ssize_t)(sizeof(bytes) - 1) ? "REREAD_PRIME_APPEND" : "MUTATION_ERROR", written == (ssize_t)(sizeof(bytes) - 1) ? 1 : errno, parent);
            record("REREAD_PRIME_BYTES", written, parent);
        }
        return;
    }
    if (strcmp(path, parent)) return;
    if (!strcmp(boundary, "read")) {
        reread_parent_data = 1;
        record("REREAD_PARENT_DATA", 1, parent);
    }
    // 真实 fstat 已得到重读后的同一打开句柄快照，回调发生在其结果交还 source 之前。
    if (!strcmp(boundary, "snapshot") && reread_parent_data && snapshot) {
        reread_phase = 2;
        record("REREAD_SNAPSHOT_BARRIER", 1, parent);
        struct stat reference = *snapshot;
        parent_reference(parent, &reference);
        if (!strcmp(mode, "observe_parent_reread")) {
            record("REREAD_NO_MUTATION", 1, parent);
            return;
        }
        const char *from = getenv("H08_IO_REPLACEMENT");
        const char *to = getenv("H08_IO_DESTINATION");
        int result = !strcmp(mode, "reread_parent_generation") ?
            (from && to ? rename(from, to) : -1) : rewrite_parent_in_place(parent, from, &reference);
        record(result == 0 ? "REREAD_MUTATION_APPLIED" : "MUTATION_ERROR", result == 0 ? 1 : errno, parent);
        if (result == 0 && !strcmp(mode, "reread_parent_generation")) record("SWAP", 1, parent);
    }
}

static void maybe_replace(const char *boundary, const char *path, const struct stat *snapshot) {
    const char *target = getenv("H08_IO_TARGET");
    const char *mode = getenv("H08_IO_REPLACE_MODE");
    if (!target || !mode) return;
    if (!strcmp(mode, "detail_after_read_replace")) {
        if (!swapped && !strcmp(path, target) && !strcmp(boundary, "read") && snapshot) {
            swapped = 1;
            record("DETAIL_FIRST_READ_INODE", snapshot->st_ino, path);
            const char *from = getenv("H08_IO_REPLACEMENT");
            int result = from ? rename(from, target) : -1;
            record(result == 0 ? "SWAP" : "SWAP_ERROR", result == 0 ? 1 : errno, path);
        }
        return;
    }
    if (!strcmp(mode, "inplace_after_data")) {
        const char *arm = getenv("H08_IO_ARM_FILE");
        if (!swapped && !strcmp(path, target) && !strcmp(boundary, "read") &&
            arm && access(arm, F_OK) == 0 && header_prechecked && snapshot && header_opened &&
            header_data_pending && snapshot->st_dev == header_dev && snapshot->st_ino == header_ino) {
            swapped = 1;
            record("INPLACE_AFTER_OPEN_BARRIER", snapshot->st_ino, path);
            parent_reference(path, snapshot);
            const char *from = getenv("H08_IO_REPLACEMENT");
            int result = rewrite_parent_in_place(path, from, snapshot);
            record(result == 0 ? "INPLACE_MUTATION_APPLIED" : "MUTATION_ERROR", result == 0 ? 1 : errno, path);
        }
        return;
    }
    if (!strcmp(mode, "parent_before_leaf")) {
        if (!strcmp(boundary, "openat-leaf") && !swapped) {
            const char *from = getenv("H08_IO_REPLACEMENT");
            const char *to = getenv("H08_IO_DESTINATION");
            int result = from && to ? rename(from, to) : -1;
            swapped = 1;
            record(result == 0 ? "SWAP" : "SWAP_ERROR", result == 0 ? 1 : errno, to ? to : path);
        }
        return;
    }
    if (!strcmp(mode, "reread_parent_generation") || !strcmp(mode, "reread_parent_ctime") ||
        !strcmp(mode, "observe_parent_reread")) {
        reread_boundary(mode, boundary, path, target, snapshot);
        return;
    }
    // 等父、子两个真实 header 均完成读取后再替换父代际，不依赖 read_dir 顺序。
    if (!strcmp(mode, "parent_generation") || !strcmp(mode, "observe_parent_chain") ||
        !strcmp(mode, "unrelated_append") || !strcmp(mode, "observe_unrelated_append")) {
        const char *parent = getenv("H08_IO_PARENT_TARGET");
        if (!parent || parent_barrier_recorded) return;
        if (!strcmp(boundary, "read")) {
            if (!strcmp(path, target)) target_read = 1;
            if (!strcmp(path, parent)) parent_read = 1;
        }
        if (!strcmp(boundary, "snapshot")) {
            if (!strcmp(path, target) && target_read) child_postchecked = 1;
            if (!strcmp(path, parent) && parent_read) parent_postchecked = 1;
        }
        if (child_postchecked && parent_postchecked) {
            parent_barrier_recorded = 1;
            int unrelated = !strcmp(mode, "unrelated_append") || !strcmp(mode, "observe_unrelated_append");
            record(unrelated ? "UNRELATED_APPEND_BARRIER" : "PARENT_CHAIN_BARRIER", 1, parent);
            if (!strcmp(mode, "unrelated_append")) {
                const char bytes[] = "{\"type\":\"probe/body\"}\n";
                int fd = open(parent, O_WRONLY | O_APPEND);
                ssize_t written = fd < 0 ? -1 : write(fd, bytes, sizeof(bytes) - 1);
                if (fd >= 0) close(fd);
                record("APPEND_BYTES", written, parent);
                record(written == (ssize_t)(sizeof(bytes) - 1) ? "APPEND_APPLIED" : "MUTATION_ERROR", written == (ssize_t)(sizeof(bytes) - 1) ? 1 : errno, parent);
            }
            if (!strcmp(mode, "parent_generation")) {
                const char *from = getenv("H08_IO_REPLACEMENT");
                const char *to = getenv("H08_IO_DESTINATION");
                int result = from && to ? rename(from, to) : -1;
                swapped = 1;
                record(result == 0 ? "SWAP" : "SWAP_ERROR", result == 0 ? 1 : errno, parent);
            }
        }
        return;
    }
    if (swapped || strcmp(path, target)) return;
    int trigger = 0;
    if (!strcmp(boundary, "read")) target_read = 1;
    if (!strcmp(boundary, "snapshot") && target_read) {
        record("HEADER_POSTCHECK", 1, path);
    }
    if (!strcmp(boundary, "openat-leaf") &&
        (!strcmp(mode, "link") || !strcmp(mode, "ancestor_leaf") || !strcmp(mode, "fifo") ||
         !strcmp(mode, "generation") || !strcmp(mode, "file_after_header"))) trigger = 1;
    if (!strcmp(boundary, "openat-ancestor") && !strcmp(mode, "ancestor")) trigger = 1;
    if (!trigger) return;
    swapped = 1;
    const char *replacement = getenv("H08_IO_REPLACEMENT");
    const char *destination = getenv("H08_IO_DESTINATION");
    int result = -1;
    if (!strcmp(mode, "fifo") && destination) {
        // 不打开 FIFO，也不创建写端；用父测试的严格超时监督真实 openat。
        if (unlink(target) == 0) result = mkfifo(destination, 0600);
        struct stat metadata;
        if (result == 0 && lstat(destination, &metadata) == 0 && S_ISFIFO(metadata.st_mode)) {
            record("FIFO_CREATED", metadata.st_ino, destination);
        }
    } else if (replacement && destination) {
        if (!strcmp(mode, "generation") || !strcmp(mode, "file_after_header")) {
            result = rename(replacement, destination);
        } else if (!strcmp(mode, "ancestor") || !strcmp(mode, "ancestor_leaf")) {
            const char *parked = getenv("H08_IO_PARKED");
            if (parked && rename(destination, parked) == 0) {
                result = symlink(replacement, destination);
            }
        } else if (unlink(target) == 0) {
            result = symlink(replacement, destination);
        }
    }
    record(result == 0 ? "SWAP" : "SWAP_ERROR", result == 0 ? 1 : errno, path);
}

static int root_directory_boundary(const char *path, int kind) {
    const char *root = getenv("H08_IO_SESSIONS_ROOT");
    const char *mode = getenv("H08_IO_REPLACE_MODE");
    const char *arm = getenv("H08_IO_ARM_FILE");
    if (!root || !mode || !arm || access(arm, F_OK) != 0 || strcmp(root, path) ||
        !sessions_root_validated || root_barrier_recorded ||
        (strcmp(mode, "sessions_directory") && strcmp(mode, "observe_sessions_directory"))) return 0;
    root_barrier_recorded = 1;
    record("SESSIONS_ROOT_IO_BEFORE", kind, path);
    record("SESSIONS_ROOT_VALIDATED_INODE", root_validated_inode, path);
    if (!strcmp(mode, "sessions_directory")) {
        const char *from = getenv("H08_IO_REPLACEMENT");
        const char *parked = getenv("H08_IO_PARKED");
        int result = -1;
        if (from && parked && rename(path, parked) == 0) result = rename(from, path);
        swapped = 1;
        record(result == 0 ? "SWAP" : "SWAP_ERROR", result == 0 ? 1 : errno, path);
    }
    struct stat metadata;
    if (lstat(path, &metadata) == 0) record("SESSIONS_ROOT_CURRENT_INODE", metadata.st_ino, path);
    return 1;
}

static DIR *probe_opendir(const char *path) {
    int boundary = root_directory_boundary(path, 1);
    DIR *result = opendir(path);
    int saved_errno = errno;
    if (boundary) record("SESSIONS_ROOT_IO_RESULT", result ? 1 : 0, path);
    errno = saved_errno;
    return result;
}

// 成员集合末验边界：武装后对 sessions 根的第 2 次真实枚举即「末验枚举」，
// 在此之前（本进程第 1 次枚举 = 扫描）已完成。此时新增一个会话目录，
// 模拟「扫描之后、发布之前」出现的候选；它不在任何逐行重开范围内。
static int root_enum_armed = 0;
static int membership_boundary(const char *path) {
    const char *root = getenv("H08_IO_SESSIONS_ROOT");
    const char *mode = getenv("H08_IO_REPLACE_MODE");
    const char *arm = getenv("H08_IO_ARM_FILE");
    if (!root || !mode || !arm ||
        (strcmp(mode, "membership_add") && strcmp(mode, "membership_swap")) ||
        strcmp(root, path) ||
        access(arm, F_OK) != 0 || swapped) return 0;
    if (++root_enum_armed != 2) return 0;
    swapped = 1;
    const char *from = getenv("H08_IO_REPLACEMENT");
    const char *to = getenv("H08_IO_DESTINATION");
    const char *parked = getenv("H08_IO_PARKED");
    int result = -1;
    if (!strcmp(mode, "membership_swap")) {
        // 同名换实体：先把原目录挪走，再把替换目录放到**同一个名字**上。
        if (parked && rename(to, parked) == 0 && from) result = rename(from, to);
    } else {
        result = from && to ? rename(from, to) : -1;
    }
    record(result == 0 ? "MEMBERSHIP_MUTATION_APPLIED" : "MUTATION_ERROR",
           result == 0 ? 1 : errno, to ? to : path);
    return 1;
}

static DIR *probe_fdopendir(int fd) {
    char path[PATH_MAX];
    int boundary = fcntl(fd, F_GETPATH, path) == 0 ? root_directory_boundary(path, 2) : 0;
    if (boundary == 0 && fcntl(fd, F_GETPATH, path) == 0) {
        membership_boundary(path);
    }
    DIR *result = fdopendir(fd);
    int saved_errno = errno;
    if (boundary) record("SESSIONS_ROOT_IO_RESULT", result ? 1 : 0, path);
    errno = saved_errno;
    return result;
}

// 仅目标压缩输入的真实读取之后开启分配观测；记录 >=1MiB 请求，不按 RSS 猜测。
// 没有采样记录只表示没有 >=1MiB 请求，不表示解码器零分配。
static void allocation_request(size_t bytes) {
    if (bytes < 1024 * 1024 || !atomic_load(&allocation_scope_active)) return;
    if (atomic_exchange(&allocation_recording, 1)) return;
    const char *target = getenv("H08_IO_ALLOCATION_TARGET");
    if (target) record("ZSTD_ALLOCATION_REQUEST", (long long)bytes, target);
    atomic_store(&allocation_recording, 0);
}
static void *probe_malloc(size_t bytes) {
    allocation_request(bytes);
    return malloc(bytes);
}
static void *probe_calloc(size_t count, size_t size) {
    allocation_request(size && count > SIZE_MAX / size ? SIZE_MAX : count * size);
    return calloc(count, size);
}
static void *probe_realloc(void *memory, size_t bytes) {
    allocation_request(bytes);
    return realloc(memory, bytes);
}

static void observed_bytes(int fd, ssize_t count) {
    if (count <= 0) return;
    const char *root = getenv("H08_IO_ROOT");
    const char *trace = getenv("H08_IO_TRACE");
    char path[PATH_MAX];
    if (!root || fcntl(fd, F_GETPATH, path) < 0) return;
    size_t size = strlen(root);
    if (strncmp(path, root, size) || path[size] != '/') return;
    if (trace && !strcmp(path, trace)) return;
    record("READ", count, path);
    struct stat metadata;
    int has_metadata = fstat(fd, &metadata) == 0;
    if (has_metadata) {
        char identity_path[PATH_MAX + 32];
        int length = snprintf(identity_path, sizeof(identity_path), "%llu:%s", (unsigned long long)metadata.st_ino, path);
        if (length > 0 && (size_t)length < sizeof(identity_path)) {
            record("READ_ID", count, identity_path);
        }
        const char *target = getenv("H08_IO_TARGET");
        const char *parent = getenv("H08_IO_PARENT_TARGET");
        if (target && !strcmp(path, target) && header_opened && metadata.st_dev == header_dev && metadata.st_ino == header_ino) {
            header_data_pending = 1;
            record("HEADER_DATA", metadata.st_ino, path);
        }
        if (parent && !strcmp(path, parent) && parent_opened && metadata.st_dev == parent_dev && metadata.st_ino == parent_ino) {
            parent_data_pending = 1;
            record("PARENT_HEADER_DATA", metadata.st_ino, path);
        }
    }
    const char *allocation_target = getenv("H08_IO_ALLOCATION_TARGET");
    if (allocation_target && !strcmp(path, allocation_target) &&
        !atomic_exchange(&allocation_scope_active, 1)) {
        record("ZSTD_ALLOCATION_SCOPE", 1, path);
    }
    maybe_replace("read", path, has_metadata ? &metadata : NULL);
}

static ssize_t probe_read(int fd, void *buffer, size_t size) {
    // dyld 不将 interpose 再应用到声明该 tuple 的镜像，因此本调用进入原函数。
    ssize_t result = read(fd, buffer, size);
    int saved_errno = errno;
    observed_bytes(fd, result);
    errno = saved_errno;
    return result;
}

static ssize_t probe_pread(int fd, void *buffer, size_t size, off_t offset) {
    ssize_t result = pread(fd, buffer, size, offset);
    int saved_errno = errno;
    observed_bytes(fd, result);
    errno = saved_errno;
    return result;
}

static ssize_t probe_readv(int fd, const struct iovec *buffers, int count) {
    ssize_t result = readv(fd, buffers, count);
    int saved_errno = errno;
    observed_bytes(fd, result);
    errno = saved_errno;
    return result;
}

static int probe_openat(int dirfd, const char *name, int flags, ...) {
    mode_t creation_mode = 0;
    if (flags & O_CREAT) {
        va_list arguments;
        va_start(arguments, flags);
        creation_mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    const char *target = getenv("H08_IO_TARGET");
    const char *parent = getenv("H08_IO_PARENT_TARGET");
    const char *mode = getenv("H08_IO_REPLACE_MODE");
    const char *ancestor = getenv("H08_IO_DESTINATION");
    const char *arm = getenv("H08_IO_ARM_FILE");
    char directory[PATH_MAX], candidate[PATH_MAX];
    int candidate_valid = 0, barrier = 0;
    if (fcntl(dirfd, F_GETPATH, directory) == 0) {
        int length = snprintf(candidate, sizeof(candidate), "%s%s%s", directory,
            !strcmp(directory, "/") ? "" : "/", name);
        candidate_valid = length > 0 && (size_t)length < sizeof(candidate);
    }
    // 武装依据是实际 openat 的 regular inode 曾被读取，随后该句柄完成 fstat 快照。
    // 栅栏只在下一次真实 openat 前执行；不依赖 pathname lstat，不更改 dirfd/flags。
    if (candidate_valid && target && mode && arm && access(arm, F_OK) == 0 &&
        !swapped && !header_openat_recorded && header_prechecked &&
        (flags & O_ACCMODE) == O_RDONLY && !(flags & O_CREAT)) {
        if (!strcmp(mode, "parent_before_leaf") && parent && !(flags & O_DIRECTORY) &&
            ((!strcmp(candidate, target) && armed_parent_snapshot) ||
             (!strcmp(candidate, parent) && armed_child_snapshot))) {
            barrier = 3;
        } else if (!strcmp(candidate, target) &&
            (!strcmp(mode, "link") || !strcmp(mode, "ancestor_leaf") || !strcmp(mode, "observe_openat") ||
             !strcmp(mode, "fifo") || !strcmp(mode, "generation") || !strcmp(mode, "file_after_header"))) {
            barrier = 1;
        } else if (!strcmp(mode, "ancestor") && ancestor && !strcmp(candidate, ancestor) && (flags & O_DIRECTORY)) {
            barrier = 2;
        }
    }
    if (barrier) {
        struct stat metadata;
        header_openat_recorded = 1;
        record(barrier == 1 ? "OPENAT_LEAF_BEFORE" : barrier == 2 ? "OPENAT_ANCESTOR_BEFORE" : "PARENT_PREOPEN_BARRIER", flags, candidate);
        if (fstat(dirfd, &metadata) == 0) record("OPENAT_PARENT_BEFORE", metadata.st_ino, candidate);
        maybe_replace(barrier == 2 ? "openat-ancestor" : "openat-leaf", target, NULL);
    }
    int result = openat(dirfd, name, flags, creation_mode);
    int saved_errno = errno;
    if (result >= 0 && candidate_valid && !(flags & O_DIRECTORY)) {
        struct stat metadata;
        if (fstat(result, &metadata) == 0 && S_ISREG(metadata.st_mode)) {
            if (target && !strcmp(candidate, target)) {
                header_opened = 1; header_dev = metadata.st_dev; header_ino = metadata.st_ino;
                header_data_pending = 0;
                record("HEADER_OPENAT_OK", metadata.st_ino, candidate);
            }
            if (parent && !strcmp(candidate, parent)) {
                parent_opened = 1; parent_dev = metadata.st_dev; parent_ino = metadata.st_ino;
                parent_data_pending = 0;
                record("PARENT_OPENAT_OK", metadata.st_ino, candidate);
            }
        }
    }
    if (barrier) {
        struct stat metadata;
        record("OPENAT_RESULT", result, candidate);
        if (result < 0) record("OPENAT_ERRNO", saved_errno, candidate);
        if (fstat(dirfd, &metadata) == 0) record("OPENAT_PARENT_AFTER", metadata.st_ino, candidate);
        if (result >= 0 && fstat(result, &metadata) == 0) record("OPENAT_FILE_INODE", metadata.st_ino, candidate);
    }
    errno = saved_errno;
    return result;
}

static int probe_fstat(int fd, struct stat *buffer) {
    int result = fstat(fd, buffer);
    int saved_errno = errno;
    char path[PATH_MAX];
    if (result == 0 && fcntl(fd, F_GETPATH, path) == 0) {
        const char *root = getenv("H08_IO_SESSIONS_ROOT");
        if (root && !strcmp(root, path) && S_ISDIR(buffer->st_mode)) {
            sessions_root_validated = 1;
            root_validated_inode = buffer->st_ino;
            record("SESSIONS_ROOT_VALIDATED", buffer->st_ino, path);
        }
        const char *target = getenv("H08_IO_TARGET");
        if (target && !strcmp(target, path) && S_ISREG(buffer->st_mode) &&
            header_opened && buffer->st_dev == header_dev && buffer->st_ino == header_ino) {
            record("HEADER_FSTAT_CTIME_SEC", buffer->st_ctimespec.tv_sec, path);
            record("HEADER_FSTAT_CTIME_NSEC", buffer->st_ctimespec.tv_nsec, path);
        }
        if (target && !strcmp(target, path) && S_ISREG(buffer->st_mode) &&
            header_opened && header_data_pending && buffer->st_dev == header_dev && buffer->st_ino == header_ino) {
            header_data_pending = 0;
            header_prechecked = 1;
            record("HEADER_PRECHECK", buffer->st_ino, path);
            record("HEADER_SNAPSHOT_CTIME_SEC", buffer->st_ctimespec.tv_sec, path);
            record("HEADER_SNAPSHOT_CTIME_NSEC", buffer->st_ctimespec.tv_nsec, path);
            const char *arm = getenv("H08_IO_ARM_FILE");
            if (arm && access(arm, F_OK) == 0) {
                armed_child_snapshot = 1;
                record("ARMED_CHILD_SNAPSHOT", buffer->st_ino, path);
            }
            maybe_replace("snapshot", path, buffer);
        }
        const char *parent = getenv("H08_IO_PARENT_TARGET");
        if (parent && !strcmp(parent, path) && S_ISREG(buffer->st_mode) &&
            parent_opened && parent_data_pending && buffer->st_dev == parent_dev && buffer->st_ino == parent_ino) {
            parent_data_pending = 0;
            record("PARENT_SNAPSHOT", buffer->st_ino, path);
            const char *arm = getenv("H08_IO_ARM_FILE");
            if (arm && access(arm, F_OK) == 0) {
                armed_parent_snapshot = 1;
                record("ARMED_PARENT_SNAPSHOT", buffer->st_ino, path);
            }
            maybe_replace("snapshot", path, buffer);
        }
        const char *allocation_target = getenv("H08_IO_ALLOCATION_TARGET");
        if (allocation_target && !strcmp(allocation_target, path) &&
            atomic_exchange(&allocation_scope_active, 0)) record("ZSTD_ALLOCATION_SCOPE", 0, path);
    }
    errno = saved_errno;
    return result;
}

#define INTERPOSE(replacement, original) \
    __attribute__((used)) static const struct { const void *new_function; const void *old_function; } \
    tuple_##original __attribute__((section("__DATA,__interpose"))) = { \
        (const void *)(uintptr_t)&replacement, (const void *)(uintptr_t)&original \
    }

INTERPOSE(probe_read, read);
INTERPOSE(probe_pread, pread);
INTERPOSE(probe_readv, readv);
INTERPOSE(probe_fstat, fstat);
INTERPOSE(probe_openat, openat);
INTERPOSE(probe_opendir, opendir);
INTERPOSE(probe_fdopendir, fdopendir);
INTERPOSE(probe_malloc, malloc);
INTERPOSE(probe_calloc, calloc);
INTERPOSE(probe_realloc, realloc);
