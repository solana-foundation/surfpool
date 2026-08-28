# Verdicts for the startup lifecycle model. `make check` runs every
# configuration documented in startup.pml's header and asserts its
# verdict; a clean pass prints one PASS line per verdict.
#
# The cells file (startup_cells.pml) is generated from the spec tables;
# `make cells` regenerates it through cargo and fails on drift. The
# same freshness is asserted by `cargo test -p surfpool-types`
# (the_pml_cells_are_current), so CI catches a stale file without spin.
#
# Gotcha: spin silently drops -D flags that follow -search, so flags
# come first in every recipe.

SPIN ?= spin

# $(1) name, $(2) -D flags, $(3) extra flags after -search, $(4) meaning.
# Arguments must avoid commas and single quotes.
define check_clean
	@out=$$($(SPIN) $(2) -search $(3) startup.pml 2>&1); \
	if echo "$$out" | grep -q "errors: 0"; then \
		echo "PASS startup/$(1): $(4)"; \
	else \
		echo "FAIL startup/$(1): expected errors: 0"; \
		echo "$$out" | tail -25; exit 1; \
	fi
endef

# $(1) name, $(2) -D flags, $(3) a symbol from the assert, $(4) meaning
define check_assert
	@out=$$($(SPIN) $(2) -search startup.pml 2>&1); \
	if echo "$$out" | grep -q "assertion violated" \
		&& echo "$$out" | grep -q "$(3)"; then \
		echo "PASS startup/$(1): $(4)"; \
	else \
		echo "FAIL startup/$(1): expected an assertion violation naming $(3)"; \
		echo "$$out" | tail -25; exit 1; \
	fi
endef

.PHONY: all check cells clean

all: check

check: ## every documented verdict
	$(call check_clean,steady,,,every schedule of sealing and concurrent task runs ends startup Ready or Failed)
	$(call check_clean,ungated,-DUNGATED,,the terminal table rows keep the projection stable without the not-over gate)
	$(call check_assert,skipstart,-DSKIPSTART,plan_state,a completion that skips its start is refused by the cells and startup never finishes)
	$(call check_clean,ready-stable,-DLTL,-a -ltl ready_stable,readiness once observed is never observed absent)
	$(call check_clean,completes,-DLTL,-a -f -ltl completes,every fair schedule reaches Ready or Failed)
	@rm -f pan pan.* *.trail _spin_nvr.tmp

cells: ## regenerate the cells from the spec tables and fail on drift
	cd ../../../.. && cargo surfpool-update-startup-pml
	git diff --exit-code startup_cells.pml

clean: ## remove the compiled verifier, spin scratch files, and trails
	rm -f pan pan.* *.trail _spin_nvr.tmp
