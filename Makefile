.PHONY: check rust-check go-check e2e fmt app

check: rust-check go-check e2e

rust-check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

go-check:
	cd sdk/go && test -z "$$(gofmt -l .)" && go vet ./... && go test -race ./...
	cd examples/service && test -z "$$(gofmt -l .)" && go vet ./... && go test -race ./...

# The Go SDK against the real `moil` binary and real uv. -count=1: the
# results depend on the moil binary, which go test's cache doesn't track.
e2e:
	cargo build -p moil-cli
	cd e2e && test -z "$$(gofmt -l .)" && go vet ./... && go test -race -count=1 ./...

fmt:
	cargo fmt --all
	cd sdk/go && gofmt -w .
	cd e2e && gofmt -w .
