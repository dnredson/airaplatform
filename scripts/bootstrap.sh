#!/bin/bash
set -euo pipefail

# AIRA Platform - Bootstrap
# - Checks Rust toolchain (rustc/cargo)
# - Installs Rust via rustup if missing
# - Installs common system deps for Rust builds on Linux
# - Runs cargo build

log() { printf "\n\033[1;34m[aira]\033[0m %s\n" "$*"; }
warn() { printf "\n\033[1;33m[aira:warn]\033[0m %s\n" "$*"; }
err() { printf "\n\033[1;31m[aira:error]\033[0m %s\n" "$*"; }

need_cmd() {
	command -v "$1" >/dev/null 2>&1
}

detect_os() {
	uname -s | tr '[:upper:]' '[:lower:]'
}

install_rust() {
	log "Rust not found. Installing via rustup..."

	if ! need_cmd curl; then
		err "curl is required to install rustup. Please install curl and re-run."
		exit 1
	fi

	# rustup official installer
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

	# Load cargo env for current shell session
	# shellcheck disable=SC1091
	source "$HOME/.cargo/env"

	log "Rust installed."
}

install_linux_deps_debian() {
	log "Installing system dependencies (Debian/Ubuntu)..."
	sudo apt-get update
	sudo apt-get install -y --no-install-recommends \
		build-essential \
		pkg-config \
		libssl-dev \
		ca-certificates \
		curl \
		git \
		clang \
		cmake
}

install_linux_deps_fedora() {
	log "Installing system dependencies (Fedora/RHEL/CentOS)..."
	sudo dnf install -y \
		@development-tools \
		pkgconf-pkg-config \
		openssl-devel \
		ca-certificates \
		curl \
		git \
		clang \
		cmake
}

install_linux_deps_arch() {
	log "Installing system dependencies (Arch)..."
	sudo pacman -Sy --noconfirm \
		base-devel \
		pkgconf \
		openssl \
		ca-certificates \
		curl \
		git \
		clang \
		cmake
}

ensure_linux_deps() {
	if need_cmd apt-get; then
		install_linux_deps_debian
	elif need_cmd dnf; then
		install_linux_deps_fedora
	elif need_cmd pacman; then
		install_linux_deps_arch
	else
		warn "Unknown Linux package manager. Skipping system deps install."
		warn "Ensure you have build tools, pkg-config and OpenSSL dev libs installed."
	fi
}

main() {
	log "Bootstrapping AIRA Platform environment..."

	OS="$(detect_os)"
	log "Detected OS: $OS"

	if [[ "$OS" == "linux" ]]; then
		if ! need_cmd sudo; then
			warn "sudo not found. If system deps are missing, install them manually."
		else
			ensure_linux_deps
		fi
	elif [[ "$OS" == "darwin" ]]; then
		warn "macOS detected. Install Xcode Command Line Tools (xcode-select --install)."
		warn "If needed, install OpenSSL and pkg-config via Homebrew: brew install openssl@3 pkg-config"
	else
		warn "Unsupported OS ($OS). Proceeding with Rust check only."
	fi

	if ! need_cmd rustc || ! need_cmd cargo; then
		install_rust
	else
		log "Rust is already installed."
	fi

	# Ensure stable toolchain and useful components
	log "Ensuring stable toolchain and common components..."
	rustup toolchain install stable >/dev/null
	rustup default stable >/dev/null
	rustup component add rustfmt clippy >/dev/null || true

	log "Tool versions:"
	rustc --version
	cargo --version
	rustup --version

	# Optional: set faster builds on dev machines (no hard changes)
	if [[ ! -f ".cargo/config.toml" ]]; then
		warn "No .cargo/config.toml found. (Optional) You can add build settings later."
	fi

	log "Running initial build..."
	cargo build

	log "Done. You can now run:"
	echo "  cargo run"
}

main "$@"
