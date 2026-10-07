CARGO ?= cargo
TARGETS ?= --all-targets
FEATURES ?=
CARGO_FLAGS = --workspace $(if $(FEATURES),--features $(FEATURES),)

.PHONY: all build check test fmt fmt-check clippy verify clean
all: build
build:
	+$(CARGO) build $(CARGO_FLAGS)
check:
	+$(CARGO) check $(CARGO_FLAGS) $(TARGETS)
test:
	+$(CARGO) test $(CARGO_FLAGS)
fmt:
	$(CARGO) fmt --all
fmt-check:
	$(CARGO) fmt --all -- --check
clippy:
	+$(CARGO) clippy $(CARGO_FLAGS) $(TARGETS) -- -D warnings
verify: fmt-check clippy test
clean:
	$(CARGO) clean
