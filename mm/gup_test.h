/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef __GUP_TEST_H
#define __GUP_TEST_H

#include <linux/types.h>

#define GUP_FAST_BENCHMARK	_IOWR('g', 1, struct gup_test)
#define PIN_FAST_BENCHMARK	_IOWR('g', 2, struct gup_test)
#define PIN_LONGTERM_BENCHMARK	_IOWR('g', 3, struct gup_test)
#define GUP_BASIC_TEST		_IOWR('g', 4, struct gup_test)
#define PIN_BASIC_TEST		_IOWR('g', 5, struct gup_test)
#define DUMP_USER_PAGES_TEST	_IOWR('g', 6, struct gup_test)
#define PIN_LONGTERM_TEST_START	_IOW('g', 7, struct pin_longterm_test)
#define PIN_LONGTERM_TEST_STOP	_IO('g', 8)
#define PIN_LONGTERM_TEST_READ	_IOW('g', 9, __u64)
#define PIN_LONGTERM_TEST_RACE	_IOWR('g', 10, struct pin_longterm_race)

#define GUP_TEST_MAX_PAGES_TO_DUMP		8

#define GUP_TEST_FLAG_DUMP_PAGES_USE_PIN	0x1

struct gup_test {
	__u64 get_delta_usec;
	__u64 put_delta_usec;
	__u64 addr;
	__u64 size;
	__u32 nr_pages_per_call;
	__u32 gup_flags;
	__u32 test_flags;
	/*
	 * Each non-zero entry is the number of the page (1-based: first page is
	 * page 1, so that zero entries mean "do nothing") from the .addr base.
	 */
	__u32 which_pages[GUP_TEST_MAX_PAGES_TO_DUMP];
};

#define PIN_LONGTERM_TEST_FLAG_USE_WRITE	1
#define PIN_LONGTERM_TEST_FLAG_USE_FAST		2
#define PIN_LONGTERM_TEST_FLAG_VERBOSE		4	/* race test: pr_info per failure */

struct pin_longterm_test {
	__u64 addr;
	__u64 size;
	__u32 flags;
};

/*
 * PIN_LONGTERM_TEST_RACE reproduces an intermittent pin_user_pages(FOLL_LONGTERM)
 * failure that occurs when a movable page carries a transient (non-FOLL_LONGTERM)
 * elevated refcount while the longterm-pin code path is attempting to migrate
 * it off of ZONE_MOVABLE / CMA. Because folio_migrate_mapping() requires the
 * refcount to match folio_expected_refs() and migrate_pages() only retries
 * NR_MAX_MIGRATE_PAGES_RETRY (10) times, an unlucky race causes the pin
 * attempt to return -ENOMEM to userspace.
 *
 * This test must be run on pages that require migration for longterm pinning
 * (i.e. pages in ZONE_MOVABLE or CMA). On x86_64, boot with e.g.
 * "movablecore=1G" or "kernelcore=..." to create a ZONE_MOVABLE region.
 */
struct pin_longterm_race {
	__u64 addr;		/* in: user VA, page-aligned */
	__u64 size;		/* in: byte length, page-aligned */
	__u32 nr_iterations;	/* in: how many times to retry FOLL_LONGTERM pin */
	__u32 flags;		/* in: PIN_LONGTERM_TEST_FLAG_USE_{WRITE,FAST} */
	__u32 nr_success;	/* out: successful pin_user_pages() iterations */
	__u32 nr_failure;	/* out: failed pin_user_pages() iterations */
	__s32 last_errno;	/* out: last errno returned from a failed pin */
	__u32 first_failure_iter; /* out: iter# (0-based) of first failure; ~0 if none */
};

#endif	/* __GUP_TEST_H */
