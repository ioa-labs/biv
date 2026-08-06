default:
    @just --list

# Build the optimized executable
build:
    cargo build --release

# Build and open an image
run image:
    cargo run --release -- "{{image}}"

# Open the print dialog at startup for debugging
debug-print image:
    BIV_DEBUG_PRINT=1 cargo run --release -- "{{image}}"

# Type-check all targets without producing an executable
check:
    cargo check --all-targets

# Run the test suite
test:
    cargo test

# Check Rust formatting
fmt:
    cargo fmt --check

# Remove Cargo build artifacts
clean:
    cargo clean
