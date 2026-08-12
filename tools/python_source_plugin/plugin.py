#!/usr/bin/env python3
"""Minimal Python SourceService plugin (disk-backed).

Requires generated stubs from source.proto (see README). Falls back to a clear
error if stubs are missing so CI without Python codegen still documents the
cross-language path.
"""

from __future__ import annotations

import argparse
import os
import sys
from concurrent import futures
from pathlib import Path

try:
    import grpc
    import source_pb2
    import source_pb2_grpc
except ImportError as e:
    print(
        "error: missing grpc stubs. Generate with:\n"
        "  python -m grpc_tools.protoc -I <proto_dir> "
        "--python_out=. --grpc_python_out=. source.proto\n"
        f"detail: {e}",
        file=sys.stderr,
    )
    sys.exit(2)


ST_OK = 0
ST_NOT_FOUND = -1
ST_NOT_A_DIR = -2
ST_IO = -4


class DiskSource(source_pb2_grpc.SourceServicer):
    def __init__(self, root: Path):
        self.root = root
        self._opens: dict[int, tuple[Path, int]] = {}
        self._next = 1

    def _resolve(self, vpath: str) -> Path:
        parts = [p for p in vpath.replace("\\", "/").split("/") if p and p != ".."]
        return self.root.joinpath(*parts) if parts else self.root

    def GetAttr(self, request, context):  # noqa: N802
        p = self._resolve(request.path)
        if not p.exists():
            return source_pb2.GetAttrResp(found=False, status=ST_OK)
        if p.is_dir():
            return source_pb2.GetAttrResp(found=True, is_dir=True, size=0, mtime=0, status=ST_OK)
        st = p.stat()
        return source_pb2.GetAttrResp(
            found=True, is_dir=False, size=st.st_size, mtime=int(st.st_mtime), status=ST_OK
        )

    def ReadDir(self, request, context):  # noqa: N802
        p = self._resolve(request.path)
        if not p.exists():
            return source_pb2.ReadDirResp(status=ST_NOT_FOUND)
        if not p.is_dir():
            return source_pb2.ReadDirResp(status=ST_NOT_A_DIR)
        entries = []
        for child in sorted(p.iterdir(), key=lambda c: c.name.lower()):
            is_dir = child.is_dir()
            size = 0 if is_dir else child.stat().st_size
            entries.append(
                source_pb2.DirEnt(name=child.name, is_dir=is_dir, size=size, mtime=0)
            )
        return source_pb2.ReadDirResp(entries=entries, status=ST_OK)

    def Open(self, request, context):  # noqa: N802
        p = self._resolve(request.path)
        if not p.is_file():
            return source_pb2.OpenResp(status=ST_NOT_FOUND)
        size = p.stat().st_size
        h = self._next
        self._next += 1
        self._opens[h] = (p, size)
        return source_pb2.OpenResp(handle=h, size=size, is_dir=False, file_id=0, status=ST_OK)

    def Read(self, request, context):  # noqa: N802
        rec = self._opens.get(request.handle)
        if rec is None:
            return source_pb2.ReadResp(status=ST_IO)
        path, size = rec
        off = request.offset
        if off >= size:
            return source_pb2.ReadResp(data=b"", status=ST_OK)
        with path.open("rb") as f:
            f.seek(off)
            data = f.read(request.len)
        return source_pb2.ReadResp(data=data, status=ST_OK)

    def Release(self, request, context):  # noqa: N802
        self._opens.pop(request.handle, None)
        return source_pb2.Empty()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--bind", default="127.0.0.1:50051")
    args = ap.parse_args()
    root = Path(args.root)
    if not root.is_dir():
        print(f"error: root not a directory: {root}", file=sys.stderr)
        sys.exit(1)

    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    source_pb2_grpc.add_SourceServicer_to_server(DiskSource(root), server)
    port = server.add_insecure_port(args.bind)
    server.start()
    host = args.bind.rsplit(":", 1)[0]
    print(f"endpoint={host}:{port}", flush=True)
    server.wait_for_termination()


if __name__ == "__main__":
    main()
