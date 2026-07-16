/* vfs-director — host API for userspace FUSE + process launch.
 *
 * PRIMARY WORKFLOW (what hosts actually do):
 *
 *   1. vfs_director_create()
 *   2. vfs_director_set_root / set_overlay / set_state_dir
 *   3. Mount content backends (vfs_director_mount / vfs_director_mount_zip)
 *   4. vfs_director_serve()     — start IPC so the child can remap I/O
 *   5. vfs_launch(...)          — CreateProcess + inject; child NT I/O under
 *                                  the virtual root is served by this director
 *   6. vfs_director_destroy()
 *
 * OPTIONAL: vfs_getattr / vfs_open / vfs_read for rare host-side inspection.
 * You do NOT need to stream game data through those; the launched process does.
 */
#ifndef VFS_DIRECTOR_H
#define VFS_DIRECTOR_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

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

/* Opaque host session (kernel + paths + IPC + launch). */
typedef struct vfs_director vfs_director;

/* ---- Backend ops (hand these to the director when mounting content) ---- */

typedef struct vfs_backend_ops {
    int (*getattr)(void *userdata, const char *path, vfs_stat *out);
    int (*readdir)(void *userdata, const char *path, void *fill_ctx,
                   int (*fill)(void *fill_ctx, const char *name, const vfs_stat *st));
    int (*open)(void *userdata, const char *path, uint32_t flags,
                uint64_t *bh_out, uint64_t *size_out, uint8_t *is_dir_out);
    int (*read)(void *userdata, uint64_t bh, uint64_t offset,
                uint8_t *buf, uint32_t len, uint32_t *nread);
    int (*release)(void *userdata, uint64_t bh);
} vfs_backend_ops;

/* ---- Lifecycle + configuration ---- */

vfs_director *vfs_director_create(void);
void vfs_director_destroy(vfs_director *d);

/* Managed game root (child cwd / remapped path prefix). */
int vfs_director_set_root(vfs_director *d, const char *path);
int vfs_director_set_overlay(vfs_director *d, const char *path);
int vfs_director_set_state_dir(vfs_director *d, const char *path);

/* Later mounts override earlier for the same path. prefix "" = entire tree. */
int vfs_director_mount(vfs_director *d, const char *prefix,
                       const vfs_backend_ops *ops, void *userdata);

/* Convenience: mount a Stored ZIP as a content backend (absolute or relative path). */
int vfs_director_mount_zip(vfs_director *d, const char *zip_path);

/* Start control-ring workers. Must succeed before vfs_launch. */
int vfs_director_serve(vfs_director *d);

/* ---- Launch (primary) ---- */

typedef struct vfs_launch_opts {
    /* Virtual image under root, e.g. "SkyrimSE.exe" or "skse64_loader.exe". */
    const char *image;
    /* Optional argv (may be NULL); image is argv0 if argc==0. */
    const char *const *argv;
    int argc;
    /* Nonzero: wait for process exit and write *exit_code. Zero: detach. */
    int wait;
    /* Nonzero: load PE from VFS and hollow (no PE file on managed root). */
    int hollow_pe;
    /* Optional absolute paths; NULL = search near host executable. */
    const char *shim_dll;
    const char *payload_dll;
} vfs_launch_opts;

/*
 * Launch a process whose I/O under the virtual root is remapped through this
 * director (inject shim + FUSE ring). Call vfs_director_serve first.
 * On wait==0, keep the director alive until the child exits.
 */
int vfs_launch(vfs_director *d, const vfs_launch_opts *opts, int32_t *exit_code);

/* ---- Optional host-side inspection (not the hot path) ---- */

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
