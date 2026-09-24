# Rudy — stable names for the commands this project already had.
#
# Every target below is a thin wrapper. The logic lives where it lived before:
# `scripts/test_suite.py` decides tier outcomes, `scripts/negative_cases.py`
# holds the refusal cases, `scripts/hardware_usb_test.py` owns the destructive
# tier and its gate. Nothing here decides anything, so nothing here can drift
# away from what the suite actually does.
#
#   make help                     what each target does
#   make test-fast                Rust + Python tests, ~30 s
#   make test                     everything non-destructive that needs no VM
#   make release-check            everything, including the VM matrix
#
# The destructive tier is deliberately not reachable by accident. See §6 of
# docs/testing-strategy.md, and `make usb-preflight` before you go near it.

SHELL := /bin/bash
.DEFAULT_GOAL := help

CARGO ?= cargo
PYTHON ?= python3
SUITE := ./scripts/run-test-suite.sh
REPORT_ROOT := target/test-reports

# The one boot case a smoke run proves: a freshly installed drive with no
# images on it. It needs no staged ISO, so it works on any bench.
SMOKE_CASE ?= empty-ntfs-gpt

.PHONY: help
help:
	@echo "Rudy test commands — see docs/testing-strategy.md §3"
	@echo
	@echo "  Fast, run these constantly"
	@echo "    make test-fast        Rust workspace tests + Python automation tests"
	@echo "    make ui-test          the GUI view-model and Slint-behaviour suite"
	@echo "    make property-test    the property and corruption suite"
	@echo
	@echo "  Before a commit"
	@echo "    make lint             clippy -D warnings, changed-file format, the payload's own target"
	@echo "    make fmt              format the files this change touched"
	@echo "    make negative         the refusal cases, against the release binaries"
	@echo "    make test             all of the above"
	@echo
	@echo "  Needs QEMU and a built payload"
	@echo "    make payload          build the boot payload from crates/rudy-boot"
	@echo "    make vm-smoke         one boot case end to end ($(SMOKE_CASE))"
	@echo "    make vm-matrix        the full image + boot matrix"
	@echo "    make release-check    everything non-destructive"
	@echo
	@echo "  Physical drive — opt-in, destructive, never in CI"
	@echo "    make usb-preflight DEVICE=/dev/sdX    inspect only, writes nothing"
	@echo "    make usb-test DEVICE=/dev/sdX         DESTROYS THE DRIVE"
	@echo
	@echo "  Reports"
	@echo "    make report           path to the newest report directory"
	@echo "    make triage           file the newest run's failures as tickets"
	@echo "    make audit            dependency advisory scan"

# ---------------------------------------------------------------- fast tests

.PHONY: test-fast
test-fast: rust-test python-test

.PHONY: rust-test
rust-test:
	$(CARGO) test --workspace --all-targets

.PHONY: python-test
python-test:
	$(PYTHON) -m unittest discover -s scripts/tests -t .

.PHONY: ui-test
ui-test:
	$(CARGO) test -p rudy-gui

.PHONY: property-test
property-test:
	$(CARGO) test -p rudy-core --test property_test
	$(CARGO) test -p rudy-core --test conformance_test

# --------------------------------------------------------------------- lint

.PHONY: lint
lint: clippy fmt-check boot-check

.PHONY: clippy
clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt-check
fmt-check:
	./scripts/check-fmt-changed.sh

.PHONY: fmt
fmt:
	./scripts/check-fmt-changed.sh --fix

# The boot payload is built for another target, so `clippy --workspace` never
# looks at the half of it that talks to firmware. This does.
#
# `--all-targets` is deliberately absent: the UEFI target ships a std, so a test
# harness built for it defines `#[panic_handler]` too and the payload's own is
# rejected as a duplicate lang item. The payload's tests are the host ones —
# `cargo test -p rudy-boot` — which is the point of the split.
#
# Skipped out loud rather than silently when the target is not installed, the
# same rule the rest of the lint tier follows for a tool it cannot find.
.PHONY: boot-check
boot-check:
	@if rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-uefi; then \
		$(CARGO) clippy -p rudy-boot --target x86_64-unknown-uefi -- -D warnings && \
		echo "[*] boot payload lints clean for x86_64-unknown-uefi"; \
	else \
		echo "[!] the x86_64-unknown-uefi target is not installed — the boot payload is UNCHECKED here."; \
		echo "    rustup target add x86_64-unknown-uefi"; \
	fi

# ---------------------------------------------------------------- negative

.PHONY: negative
negative: release-binaries
	$(PYTHON) -m scripts.negative_cases

# ------------------------------------------------------------- the full set

.PHONY: test
test: lint test-fast negative
	@echo
	@echo "[*] every non-destructive check that needs no VM has passed."
	@echo "    This is not boot evidence — see docs/testing-strategy.md §2."

# ------------------------------------------------------------------ the VM

.PHONY: payload
payload:
	./scripts/build-boot-payload.sh

.PHONY: release-binaries
release-binaries:
	$(CARGO) build --release --bin rudy

.PHONY: vm-smoke
vm-smoke:
	$(SUITE) --tier image --tier boot --case $(SMOKE_CASE)

.PHONY: vm-matrix
vm-matrix:
	$(SUITE) --tier image --tier boot

.PHONY: release-check
release-check:
	$(SUITE)
	@echo
	@echo "[!] Two things this cannot tell you, both in docs/testing-strategy.md §8:"
	@echo "    - open 03_settled.png for every newly-green boot case. The settle"
	@echo "      check cannot tell an installer from a rescue shell, and has been"
	@echo "      wrong twice."
	@echo "    - the hardware tier has not run. It never runs from here."

# ------------------------------------------------------- the physical drive

# Named twice, exactly as the harness requires, and only from a DEVICE the
# operator spelled out. There is no default: "first removable disk" is how the
# wrong disk gets erased.
#
# Both guards are the first lines of the recipe rather than prerequisites.
# `make -j` runs prerequisites in parallel, so a `require-device` sitting beside
# `release-binaries` let a four-second release build run to completion before
# the refusal was printed — and the refusal then scrolled past inside cargo's
# output. A guard that runs concurrently with the thing it guards is decoration.
define require_device
	@if [ -z "$(DEVICE)" ]; then \
		echo "[!] DEVICE is not set."; \
		echo "    Name the device explicitly: make $@ DEVICE=/dev/sdX"; \
		echo "    There is no default, deliberately."; \
		exit 2; \
	fi
endef

.PHONY: usb-preflight
usb-preflight:
	$(require_device)
	@$(MAKE) --no-print-directory release-binaries
	$(PYTHON) scripts/hardware_usb_test.py \
		--device $(DEVICE) --confirm-wipe-disk $(DEVICE) \
		--preflight-only --report-dir target/hardware-preflight

.PHONY: usb-test
usb-test:
	$(require_device)
	@if [ "$$ALLOW_DESTRUCTIVE_USB_TESTS" != "1" ]; then \
		echo "[!] ALLOW_DESTRUCTIVE_USB_TESTS is not set to 1 in this shell."; \
		echo "    The harness will refuse anyway; this saves you the build."; \
		echo "      ALLOW_DESTRUCTIVE_USB_TESTS=1 make usb-test DEVICE=$(DEVICE)"; \
		exit 2; \
	fi
	@echo "This DESTROYS every byte on $(DEVICE)."
	@echo "Run 'make usb-preflight DEVICE=$(DEVICE)' first if you have not."
	@echo
	@$(MAKE) --no-print-directory release-binaries
	$(PYTHON) scripts/hardware_usb_test.py \
		--device $(DEVICE) --confirm-wipe-disk $(DEVICE) \
		--report-dir target/hardware-test $(if $(ISO),--iso $(ISO),)

# ------------------------------------------------------------------ reports

# Files each failure from an existing run as a needs-triage ticket. The suite
# does this itself; this is for re-filing from a run that used --no-file-tickets.
.PHONY: triage
triage:
	@latest=$$(ls -1d $(REPORT_ROOT)/*/ 2>/dev/null | sort | tail -1); \
	if [ -z "$$latest" ]; then \
		echo "[!] no runs under $(REPORT_ROOT) to triage"; exit 1; \
	else \
		$(PYTHON) -m scripts.triage "$$latest""results.json"; \
	fi

.PHONY: report
report:
	@latest=$$(ls -1d $(REPORT_ROOT)/*/ 2>/dev/null | sort | tail -1); \
	if [ -z "$$latest" ]; then \
		echo "[!] no reports under $(REPORT_ROOT) yet — run 'make test' or 'make vm-smoke'"; \
	else \
		echo "$$latest"; \
		echo "  REPORT.md    $$latest""REPORT.md"; \
		echo "  results.json $$latest""results.json"; \
	fi

.PHONY: audit
audit:
	@if command -v cargo-audit > /dev/null; then \
		$(CARGO) audit; \
	else \
		echo "[!] cargo-audit is not installed, so dependencies are UNSCANNED here."; \
		echo "    Install it:  cargo install cargo-audit"; \
		echo "    CI does NOT cover you: the advisories job runs on a weekly cron,"; \
		echo "    not on push, and is continue-on-error. Nothing fails on an advisory."; \
		exit 1; \
	fi

.PHONY: clean-test-artifacts
clean-test-artifacts:
	rm -rf target/negative-cases target/hardware-preflight
	@echo "[*] left $(REPORT_ROOT) and target/suite-images alone — they are evidence"
