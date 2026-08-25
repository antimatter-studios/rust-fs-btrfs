/*
 * fs_btrfs.h — C ABI for the pure-Rust Btrfs driver.
 *
 * Link against libfs_btrfs.a. Distinct handles are independent; a single
 * handle must not be used concurrently from two threads.
 *
 * Error convention: functions returning int return 0 on success and -1
 * on failure; functions returning a pointer return NULL. In either case
 * fs_btrfs_last_error() gives a message for the calling thread and
 * fs_btrfs_last_errno() a POSIX errno suitable for returning to a
 * filesystem client.
 *
 * The driver is read-only, and refuses rather than guesses. A compressed
 * extent fails with ENOTSUP rather than returning its undecoded bytes,
 * because a caller cannot distinguish those from a corrupt file.
 */

#ifndef FS_BTRFS_H
#define FS_BTRFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct fs_btrfs_fs fs_btrfs_fs_t;
typedef struct fs_btrfs_dir_iter fs_btrfs_dir_iter_t;

/* File types, matching the values used by the sibling drivers. */
typedef enum {
    FS_BTRFS_FT_UNKNOWN = 0,
    FS_BTRFS_FT_REGULAR = 1,
    FS_BTRFS_FT_DIRECTORY = 2,
    FS_BTRFS_FT_CHARDEV = 3,
    FS_BTRFS_FT_BLOCKDEV = 4,
    FS_BTRFS_FT_FIFO = 5,
    FS_BTRFS_FT_SOCKET = 6,
    FS_BTRFS_FT_SYMLINK = 7
} fs_btrfs_file_type_t;

typedef struct {
    uint64_t inode;
    uint32_t mode;        /* permission bits and type, as on disk */
    uint32_t uid;
    uint32_t gid;
    uint64_t size;
    uint64_t nbytes;      /* bytes actually allocated */
    int64_t  atime;       /* unix epoch seconds */
    int64_t  mtime;
    int64_t  ctime;
    int64_t  otime;       /* creation time */
    uint32_t link_count;
    uint32_t file_type;   /* fs_btrfs_file_type_t */
} fs_btrfs_attr_t;

typedef struct {
    uint64_t inode;
    uint8_t  file_type;   /* fs_btrfs_file_type_t */
    uint8_t  name_len;
    /*
     * Non-zero when this entry names a SUBVOLUME rather than an inode.
     * Btrfs directory entries may point at either, and `inode` then
     * holds a tree id. A caller that treats it as an inode number will
     * look up nonsense, so check this before using it.
     */
    uint8_t  is_subvolume;
    char     name[256];   /* NUL-terminated */
} fs_btrfs_dirent_t;

typedef struct {
    uint32_t sector_size;
    uint32_t node_size;
    uint64_t total_bytes;
    uint64_t bytes_used;
    uint64_t num_devices;
    uint16_t csum_type;   /* 0 crc32c, 1 xxhash64, 2 sha256, 3 blake2b */
    char     label[256];  /* NUL-terminated */
    uint8_t  fsid[16];
    uint8_t  metadata_uuid[16];
    uint64_t feature_compat;
    uint64_t feature_compat_ro;
    uint64_t feature_incompat;
} fs_btrfs_volume_info_t;

/*
 * Read callback. Must fill exactly `length` bytes at `offset` and return
 * 0, or return non-zero. A short read is a failure, not a partial
 * success.
 */
typedef int (*fs_btrfs_read_fn)(void *context, void *buf,
                                uint64_t offset, uint64_t length);

typedef struct {
    fs_btrfs_read_fn read;
    void            *context;
    uint64_t         size_bytes;
    uint32_t         block_size;
} fs_btrfs_blockdev_cfg_t;

/* ---- diagnostics ---- */

/* Never NULL, including before any failure. */
const char *fs_btrfs_last_error(void);
int fs_btrfs_last_errno(void);

/* ---- mounting ---- */

fs_btrfs_fs_t *fs_btrfs_mount(const char *device_path);
fs_btrfs_fs_t *fs_btrfs_mount_with_callbacks(const fs_btrfs_blockdev_cfg_t *cfg);
void fs_btrfs_umount(fs_btrfs_fs_t *fs);
int fs_btrfs_get_volume_info(fs_btrfs_fs_t *fs, fs_btrfs_volume_info_t *out);

/* ---- lookup and metadata ---- */

/* Symbolic links are NOT followed; describes the link itself. */
int fs_btrfs_stat(fs_btrfs_fs_t *fs, const char *path, fs_btrfs_attr_t *out);
int fs_btrfs_stat_ino(fs_btrfs_fs_t *fs, uint64_t inode, fs_btrfs_attr_t *out);

/* ---- directories ---- */

fs_btrfs_dir_iter_t *fs_btrfs_dir_open(fs_btrfs_fs_t *fs, const char *path);
/* 1 = filled, 0 = end of directory, -1 = failure. */
int fs_btrfs_dir_next(fs_btrfs_dir_iter_t *iter, fs_btrfs_dirent_t *out);
void fs_btrfs_dir_close(fs_btrfs_dir_iter_t *iter);

/* ---- file contents ---- */

/*
 * Bytes read, 0 at end of file, or -1. Holes and preallocated extents
 * read as zeros; a preallocated extent's blocks hold whatever previously
 * occupied them, so returning their contents would disclose it.
 */
int64_t fs_btrfs_read_file(fs_btrfs_fs_t *fs, const char *path,
                           uint64_t offset, void *buf, uint64_t length);

/* Length written excluding the terminator, or -1. Truncates to bufsize. */
int fs_btrfs_readlink(fs_btrfs_fs_t *fs, const char *path,
                      char *buf, size_t bufsize);

#ifdef __cplusplus
}
#endif

#endif /* FS_BTRFS_H */
