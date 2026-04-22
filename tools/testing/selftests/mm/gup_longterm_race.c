// SPDX-License-Identifier: GPL-2.0
/*
 * gup_longterm_race: reproduce intermittent pin_user_pages(FOLL_LONGTERM)
 *                    failures caused by transient refcount elevation on
 *                    migratable pages.
 *
 * Background
 * ----------
 * pin_user_pages(FOLL_LONGTERM) is required by RDMA / GPU / any driver that
 * intends to hold a DMA pin on user memory for an unbounded amount of time.
 * Before taking the longterm pin, the kernel must migrate movable pages
 * (ZONE_MOVABLE or CMA) to non-movable zones. That migration goes through
 * folio_migrate_mapping(), which requires the source folio refcount to match
 * an "expected_count". If anything else in the kernel happens to be holding
 * a transient reference on the page (e.g. a concurrent get_user_pages_fast(),
 * NUMA balancing, KSM, etc.), the refcount check fails with -EAGAIN.
 *
 * migrate_pages() only retries NR_MAX_MIGRATE_PAGES_RETRY (10) times. If the
 * transient refcount does not happen to be dropped during any of those 10
 * retries, migrate_longterm_unpinnable_folios() returns -ENOMEM to
 * pin_user_pages(FOLL_LONGTERM), which surfaces to userspace as:
 *
 *     ibv_reg_mr(...) failed: Cannot allocate memory
 *
 * That is the bug originally reported on GH200 (NVIDIA Grace Hopper) systems
 * where ZONE_MOVABLE is used to host GPU memory, and the NIC driver (mlx5,
 * EFA, ...) pins user memory via pin_user_pages(FOLL_LONGTERM). See the
 * kernel-side ioctl PIN_LONGTERM_TEST_RACE in mm/gup_test.c for the racing
 * worker that reproduces the condition.
 *
 * This test deliberately races a kernel worker (that pulses transient refs
 * via get_user_pages_fast()/release_pages()) against the main thread's
 * pin_user_pages(FOLL_LONGTERM) calls, and reports the failure rate.
 *
 * IMPORTANT: on x86_64 you must boot the kernel with e.g. "movablecore=1G"
 * to create a ZONE_MOVABLE region; otherwise check_and_migrate_movable_folios()
 * has nothing to migrate, no race exists, and the test will pass trivially.
 * The bug only manifests for pages that are not "longterm pinnable".
 */
#define __SANE_USERSPACE_TYPES__
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>

#include <mm/gup_test.h>
#include "kselftest.h"

#define GUP_TEST_FILE		"/sys/kernel/debug/gup_test"
#define DEFAULT_SIZE_MB		8
#define DEFAULT_ITERATIONS	200

static void usage(const char *argv0)
{
	fprintf(stderr,
		"Usage: %s [-m MiB] [-n iterations] [-f] [-w] [-v]\n"
		"  -m MiB         region size in MiB (default %d)\n"
		"  -n iterations  FOLL_LONGTERM pin retries (default %d)\n"
		"  -f             use pin_user_pages_fast path\n"
		"  -w             request write pins (FOLL_WRITE)\n"
		"  -v             verbose: log per-failure iter/errno to dmesg\n"
		"\n"
		"On x86_64 boot with 'movablecore=1G' (or similar) so that\n"
		"mmap() can back the region with ZONE_MOVABLE pages; the\n"
		"bug only appears for pages requiring longterm-pin migration.\n",
		argv0, DEFAULT_SIZE_MB, DEFAULT_ITERATIONS);
}

int main(int argc, char **argv)
{
	struct pin_longterm_race args = { 0 };
	size_t size_mb = DEFAULT_SIZE_MB;
	unsigned int iterations = DEFAULT_ITERATIONS;
	unsigned int flags = 0;
	size_t size, i, psize;
	char *p;
	int fd, opt, ret;

	while ((opt = getopt(argc, argv, "m:n:fwvh")) != -1) {
		switch (opt) {
		case 'm':
			size_mb = (size_t)atoi(optarg);
			break;
		case 'n':
			iterations = (unsigned int)atoi(optarg);
			break;
		case 'f':
			flags |= PIN_LONGTERM_TEST_FLAG_USE_FAST;
			break;
		case 'w':
			flags |= PIN_LONGTERM_TEST_FLAG_USE_WRITE;
			break;
		case 'v':
			flags |= PIN_LONGTERM_TEST_FLAG_VERBOSE;
			break;
		case 'h':
		default:
			usage(argv[0]);
			return opt == 'h' ? 0 : KSFT_FAIL;
		}
	}

	if (!size_mb || !iterations) {
		usage(argv[0]);
		return KSFT_FAIL;
	}

	psize = sysconf(_SC_PAGESIZE);
	size = size_mb * 1024UL * 1024UL;
	size = (size + psize - 1) & ~(psize - 1);

	ksft_print_header();
	ksft_set_plan(1);

	fd = open(GUP_TEST_FILE, O_RDWR);
	if (fd < 0) {
		if (errno == EACCES && getuid())
			ksft_test_result_skip("need root to open %s\n",
					      GUP_TEST_FILE);
		else if (errno == ENOENT)
			ksft_test_result_skip(
				"%s missing; enable CONFIG_GUP_TEST and mount debugfs\n",
				GUP_TEST_FILE);
		else
			ksft_test_result_skip("open(%s): %s\n",
					      GUP_TEST_FILE, strerror(errno));
		ksft_finished();
	}

	p = mmap(NULL, size, PROT_READ | PROT_WRITE,
		 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (p == MAP_FAILED) {
		ksft_print_msg("mmap(%zu): %s\n", size, strerror(errno));
		close(fd);
		return KSFT_FAIL;
	}

	/*
	 * Avoid THP so we exercise many base pages; the bug scales with the
	 * number of folios the migration path has to juggle.
	 */
	madvise(p, size, MADV_NOHUGEPAGE);

	/* Fault every page in so there is something concrete to race on. */
	for (i = 0; i < size; i += psize)
		p[i] = 0;

	args.addr = (unsigned long)p;
	args.size = size;
	args.nr_iterations = iterations;
	args.flags = flags;

	ksft_print_msg("addr=0x%llx size=%zu iterations=%u flags=0x%x\n",
		       args.addr, size, iterations, flags);

	ret = ioctl(fd, PIN_LONGTERM_TEST_RACE, &args);
	if (ret) {
		ksft_print_msg("PIN_LONGTERM_TEST_RACE ioctl: %s\n",
			       strerror(errno));
		munmap(p, size);
		close(fd);
		return KSFT_FAIL;
	}

	{
		unsigned int total = args.nr_success + args.nr_failure;
		double rate = total ? (100.0 * args.nr_failure) / total : 0.0;

		ksft_print_msg(
			"pin_user_pages(FOLL_LONGTERM): %u success, %u failure (%.2f%% fail)\n",
			args.nr_success, args.nr_failure, rate);
		if (args.nr_failure) {
			/*
			 * last_errno is the kernel's negative errno; strerror()
			 * expects the positive value.
			 */
			int err = args.last_errno < 0 ? -args.last_errno :
							args.last_errno;

			ksft_print_msg(
				"  first failure at iter=%u, last err=%d (%s)\n",
				args.first_failure_iter, args.last_errno,
				strerror(err));
			if (flags & PIN_LONGTERM_TEST_FLAG_VERBOSE)
				ksft_print_msg(
					"  per-failure details logged to dmesg (gup_test:)\n");
		}
	}

	/*
	 * The test PASSES only if every FOLL_LONGTERM pin succeeded.
	 * Any failure reproduces the bug: migration of a transiently-
	 * referenced movable folio returned -ENOMEM instead of waiting
	 * for the refcount to quiesce.
	 */
	ksft_test_result(args.nr_failure == 0,
			 "FOLL_LONGTERM pin robust against transient refcounts\n");

	munmap(p, size);
	close(fd);
	ksft_finished();
}
