.PHONY: check rust-check go-check e2e fmt app

check: rust-check go-check e2e

rust-check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

go-check:
	cd sdk/go && test -z "$$(gofmt -l .)" && go vet ./... && go test -race ./...
	cd examples/service && test -z "$$(gofmt -l .)" && go vet ./... && go test ./...

# The Go SDK against the real `moil` binary and real uv.
e2e:
	cargo build -p moil-cli
	cd e2e && go test ./...

fmt:
	cargo fmt --all
	cd sdk/go && gofmt -w .
	cd e2e && gofmt -w .
