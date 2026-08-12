# Python Source plugin (cross-language proof)

Implements the same gRPC `vfs.source.Source` contract as
`vfs-source-plugin` (Rust), so the director can mount a Python-backed source
via `type = "remote"`.

## Setup

```bash
python -m venv .venv
# Windows: .venv\Scripts\activate
pip install grpcio grpcio-tools
python -m grpc_tools.protoc \
  -I ../../rust/crates/vfs-source/proto \
  --python_out=. --grpc_python_out=. \
  ../../rust/crates/vfs-source/proto/source.proto
```

## Run

```bash
python plugin.py --root /path/to/files --bind 127.0.0.1:50051
```

Then in a scenario:

```toml
[[source]]
type = "remote"
endpoint = "127.0.0.1:50051"
mount = "/"
layer = 0
```
