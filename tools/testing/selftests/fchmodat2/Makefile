# SPDX-License-Identifier: GPL-2.0-or-later

CFLAGS += -Wall -O2 -g -fsanitize=address -fsanitize=undefined $(KHDR_INCLUDES)

TEST_GEN_PROGS := fchmodat2_test

include ../lib.mk

# gcc requires -static-libasan in order to ensure that Address Sanitizer's
# library is the first one loaded. However, clang already statically links the
# Address Sanitizer if -fsanitize is specified. Therefore, simply omit
# -static-libasan for clang builds.
# This check must be done after including ../lib.mk, in order to pick up the
# correct value of SELFTESTS_CC_IS_CLANG.
ifeq ($(SELFTESTS_CC_IS_CLANG),)
    CFLAGS += -static-libasan
endif
