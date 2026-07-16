/* vfs-director: userspace FUSE kernel C ABI.
 *
 * Hosts create a director, mount backends (zip, disk, custom), then
 * getattr/readdir/open/read/close like a small FUSE client — all in-process.
 * The game IPC ring is a separate client of the same kernel (Rust side).
 */
#ifndef VFS_DIRECTOR_H
#define VFS_DIRECTOR_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status: 0 = OK; negative match vfs-protocol (ST_*). */
#define VFS_OK                 0
#define VFS_ERR_NOT_FOUND     (-1)
#define VFS_ERR_NOT_A_DIR     (-2)
#define VFS_ERR_BAD_REQUEST   (-3)
#define VFS_ERR_IO            (-4)
#define VFS_ERR_IS_DIR        (-5)
#define VFS_ERR_BAD_FH        (-6)

#define VFS_KIND_FILE       1
#define VFS_KIND_DIR        2
#define VFS_KIND_TOMBSTONE  3

#define VFS_OPEN_READ  1
#define VFS_OPEN_WRITE 2

typedef struct vfs_stat {
    uint8_t kind;
    uint64_t size;
    int64_t mtime;
} vfs_stat;

typedef struct vfs_director vfs_director;

/* Backend ops: implement these for zip/disk/custom. userdata is host-owned. */
typedef struct vfs_backend_ops {
    /* 0 + out filled; VFS_ERR_NOT_FOUND if missing; other negative on error. */
    int (*getattr)(void *userdata, const char *path, vfs_stat *out);
    /* fill returns 0 to continue, non-zero to stop. */
    int (*readdir)(void *userdata, const char *path, void *fill_ctx,
                   int (*fill)(void *fill_ctx, const char *name, const vfs_stat *st));
    /* On success: *bh_out backend handle, *size_out file size (0 for dir). */
    int (*open)(void *userdata, const char *path, uint32_t flags,
                uint64_t *bh_out, uint64_t *size_out, uint8_t *is_dir_out);
    int (*read)(void *userdata, uint64_t bh, uint64_t offset,
                uint8_t *buf, uint32_t len, uint32_t *nread);
    int (*release)(void *userdata, uint64_t bh);
} vfs_backend_ops;

vfs_director *vfs_director_create(void);
void vfs_director_destroy(vfs_director *d);

/* Later mounts override earlier for the same path. prefix "" or "/" = root. */
int vfs_director_mount(vfs_director *d, const char *prefix,
                       const vfs_backend_ops *ops, void *userdata);

/* In-process FUSE-style client of the kernel. */
int vfs_getattr(vfs_director *d, const char *path, vfs_stat *out);
int vfs_readdir(vfs_director *d, const char *path, void *fill_ctx,
                int (*fill)(void *fill_ctx, const char *name, const vfs_stat *st));
int vfs_open(vfs_director *d, const char *path, uint32_t flags,
             uint64_t *fh_out, uint64_t *size_out, uint8_t *is_dir_out);
int vfs_read(vfs_director *d, uint64_t fh, uint64_t offset,
             uint8_t *buf, uint32_t len, uint32_t *nread);
int vfs_close(vfs_director *d, uint64_t fh);

#ifdef __cplusplus
}
#endif

#endif /* VFS_DIRECTOR_H */
