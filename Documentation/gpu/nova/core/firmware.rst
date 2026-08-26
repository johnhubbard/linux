.. SPDX-License-Identifier: (GPL-2.0+ OR MIT)

=============================
Firmware files and ABI epochs
=============================

Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

nova-core boots the GSP with firmware images that it requests through the
kernel firmware loader. linux-firmware distributes those images. This
document states where the files are installed, what keeps a firmware release
compatible with the kernels already released, and how the driver chooses
among installed files after an incompatible change.

The unit of compatibility is the ABI epoch. Within one epoch the filenames
never change, and every firmware release works with every kernel that
supports the epoch. A firmware change that cannot meet that rule starts a
new epoch with a new set of filenames, and the files of the old epoch stay
installed for the kernels that request them.

Epoch 0 is the only epoch that exists today, so nova-core requests one set of
filenames and has nothing to choose between. The rules below take effect when
a second epoch is added.

Compatibility
=============

The kernel and linux-firmware are released and updated independently, so a
system can pair any kernel with any firmware release. Within an epoch, every
kernel that supports the epoch must boot and operate the GPU with every
firmware release in it. That holds in both directions: a newer firmware
release with an older kernel, and a newer kernel with an older firmware
release. A feature or fix that needs both a kernel change and a firmware
change takes effect once both are installed.

:doc:`../../../driver-api/firmware/firmware-usage-guidelines` sets the
kernel-wide rules for firmware in linux-firmware. Two of them are the basis
for epochs:

- A firmware file is named with its major version only. A compatible update
  replaces the file under the same name, and every update stays compatible
  with every kernel that loads that major version.
- An incompatible change gets a new major version, and the kernel keeps
  loading the older major versions.

For nova-core firmware, the epoch number is that major version. Adding an
epoch removes nothing and deprecates nothing.

A security fix that needs both a kernel change and a firmware change is
applied to every epoch that a supported stable or longterm kernel still
requests. When an epoch cannot be made safe, a new kernel may stop loading
it, and a system running that kernel then needs firmware from an epoch the
kernel still loads. The kernel-wide guidelines allow dropping an epoch only
as a last resort, and the change that drops one states the reason.

Directory layout
================

nova-core requests its firmware from ``nvidia/<chip>/gsp/`` today. Before
the firmware is submitted to linux-firmware, the driver moves to
``nvidia/gpu/<chip>/gsp/``, and this document describes that layout. No
nova-core firmware is in linux-firmware yet, so the move affects no released
kernel. A kernel requests one of the two locations, never both, and never
falls back from one to the other.

linux-firmware installs a submitted file set below ``nvidia/gpu``, one
directory per chip. The directory is named with the lowercase chip name, and
the GSP images are in its ``gsp`` subdirectory::

    /lib/firmware/nvidia/gpu/<chip>/gsp/
        <image>.tlv
        <image>.bin

Under this layout the driver requests ``nvidia/gpu/<chip>/gsp/<image>.tlv``,
relative to the firmware search path, and the firmware loader opens the file
below ``/lib/firmware``. nouveau requests its own GSP-RM files from
``nvidia/<chip>/gsp/`` and stays there, so the ``gpu`` path component keeps
the two drivers' files in separate directories.

Every image has a ``.tlv`` file that holds its metadata. The payload is
either inside the TLV or in a ``.bin`` companion file that the TLV names.
:doc:`tlv` lists the images, their tags, and which images use a companion
file.

When several chips use the same image, linux-firmware may install one copy
and symbolic links to it. A link may replace the chip directory, the ``gsp``
subdirectory, or a single file.

ABI epochs
==========

An epoch is the complete set of files that boot a chip, together with every
interface between nova-core and the firmware in those files: the TLV tags
the driver parses, the boot arguments the driver passes to GSP-RM, the
framebuffer layout that both sides assume, and the command and message
interface used once GSP-RM runs.

Two tests decide whether a change stays in its epoch. A firmware release
stays in its epoch only if every existing kernel that supports the epoch
boots and operates with it. A kernel may claim support for an epoch only if
it works with every firmware release already published in that epoch. A
change that fails either test starts a new epoch. An addition passes both
tests when an older kernel can ignore it and a newer kernel still works with
firmware that lacks it.

The driver defines the TLV tags and derives whatever GSP-RM needs from their
values. GSP-RM defines none of them, so a GSP-RM interface change and a tag
change are separate events, and the two tests apply to each of them.
:doc:`tlv` states which tag changes stay within an epoch.

File names
==========

Epoch 0 files have no suffix. A file in a later epoch has a hyphen and the
epoch number before the extension. A chip with epochs 0 and 1 installed has
both sets in one directory::

    /lib/firmware/nvidia/gpu/<chip>/gsp/
        <image>.tlv
        <image>.bin
        <image>-1.tlv
        <image>-1.bin

Epoch numbers come from one sequence that every chip shares, and a number is
never reused, so each number identifies one incompatible change. A change
can affect some chips and not others, and an unaffected chip keeps the files
it has, so one chip's epoch numbers can have gaps. When a chip's firmware is
first submitted after epoch 2 exists, its files are named for epoch 2 and it
has no unsuffixed files.

A new epoch adds files and changes none. Every file the affected chip needs
gets the new suffix, including a file whose contents did not change, so that
an update to one epoch can never alter another. A file from one epoch is
never used with a file from another, and a path that names an epoch must
reach a file from that epoch, through any symbolic link on the way.

When a TLV keeps its payload in a companion file, its ``FILE`` tag names
that file. The name is the TLV's own basename with a ``.bin`` extension, so
``gsp-1.tlv`` names ``gsp-1.bin``. The driver reads the name from the tag at
runtime, but its ``MODULE_FIRMWARE`` entries are fixed when it is built, and
the rule lets those entries name every payload in advance.

The ``VERS`` tag inside a TLV names the firmware release, not the epoch. The
epoch is in the filename because the driver chooses among installed files
before it can open one and read its tags.

Epoch selection
===============

nova-core has a list of supported epochs for each chip. The list names
every epoch that exists for the chip when the driver is released, minus any
epoch dropped under the security exception in `Compatibility`_. The driver
tries the listed epochs from the highest number down, and it never requests
an epoch that is not on the list.

An epoch counts as installed when all of its files are present and parse:
every TLV file the chip needs, and every companion file that those TLVs
name. To check one, the driver requests each TLV file in the set, parses it,
and loads the companion file it names. The check has one of four outcomes:

- Every TLV file is missing. The epoch is not installed, and the driver
  tries the next lower epoch on its list. Only a file-not-found error counts
  as missing.
- Every file is present and parses. The driver selects the epoch.
- Some files are present and others are missing, or a file does not parse.
  The installation is broken, and the probe fails. Falling back to a lower
  epoch would hide the breakage, so the driver does not fall back.
- Any other error, such as a failed allocation, also fails the probe.

The driver requests the TLV files with ``firmware_request_nowarn``, so a
system with only epoch 0 installed logs nothing about the higher epochs that
were checked first. The driver logs the epoch it selected, or that no listed
epoch was installed. When the selected epoch is lower than the highest on
the list, the message is a warning, because the installed firmware lacks the
changes that the higher epochs carry.

After selection, the driver uses only files from the selected epoch. A
failed signature check or an image that the hardware rejects fails the
probe, with no fallback to a lower epoch.

The firmware loader offers no directory listing, so the driver learns what
is installed only by requesting names it knows. If epochs 2 and 0 are
installed, a driver that lists 2, 1, and 0 selects epoch 2. A driver that
lists 1 and 0 finds no epoch 1 file and selects epoch 0.

Module firmware metadata
========================

The ``MODULE_FIRMWARE`` entries name every TLV file and every companion file
of every listed epoch, for every chip. An initramfs generator copies the
files the entries name, and it cannot know which epoch the driver will find
installed, so every candidate set has to be listed. If a ``FILE`` tag breaks
the naming rule in `File names`_, the payload it names is absent from the
entries and from any initramfs built from them.

Cost of an epoch
================

A new epoch needs an implementation in the driver, a complete file set in
linux-firmware for every affected chip, and matching module firmware
metadata. The implementation stays tested and maintained for as long as the
driver lists the epoch.

linux-firmware keeps the files of every epoch, so each retained epoch adds
files or symbolic links to the installed tree. A distribution that puts
nova-core in its initramfs copies every file the module firmware metadata
names, so each listed epoch adds to the initramfs as well.

Retiring an epoch
=================

An epoch stays usable by every kernel that lists it, including a kernel that
prefers a higher epoch. Retiring one takes two steps that can be years
apart. A new kernel drops the epoch from its lists and removes the
implementation. linux-firmware removes the files once every kernel that
still requests them has reached end of life. Enterprise kernels can keep an
epoch in linux-firmware for a decade or more.

Propose the removal on nova-gpu@lists.linux.dev, naming the epoch, the chips
it covers, every kernel release that requests it, and each release's
end-of-life date.

A kernel may also drop an epoch under the security exception in
`Compatibility`_ without waiting. Removing the files from linux-firmware
still follows this process.
