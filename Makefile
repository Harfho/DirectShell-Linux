# DirectShell Linux Makefile

# Build configuration
RUSTC = rustc
CARGO = cargo
TARGET = release
BUILD_DIR = target/$(TARGET)
APP_NAME = directshell-linux

# Directories
SRC_DIR = src
BUILD_DIR = target/$(TARGET)
BIN_DIR = $(BUILD_DIR)
DOC_DIR = docs

# Build flags
RUSTFLAGS = -C opt-level=s

# Default target
.PHONY: all build clean install test run help

all: build

# Build the application
build:
	$(CARGO) build --$(TARGET)

# Clean build artifacts
clean:
	$(CARGO) clean

# Install the application
install:
	$(CARGO) install --path .

# Run tests
test:
	$(CARGO) test

# Run the application
run:
	$(BUILD_DIR)/$(APP_NAME)

# Show help
help:
	@echo "DirectShell Linux Makefile"
	@echo "Available targets:"
	@echo "  build    - Build the application"
	@echo "  clean    - Remove build artifacts"
	@echo "  install  - Install to system"
	@echo "  test     - Run tests"
	@echo "  run      - Run the application"
	@echo "  help     - Show this help"