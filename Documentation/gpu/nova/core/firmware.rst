.. SPDX-License-Identifier: (GPL-2.0+ OR MIT)

=============================
Firmware files and ABI epochs
=============================

Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

GSP-RM is the firmware that runs on the GSP, the GPU System Processor.
nova-core boots the GSP with GSP-RM and with the images that load GSP-RM,
and it requests each image through the kernel firmware loader. This document
sets the rules for distributing the images through linux-firmware: where the
files are installed, what keeps a firmware release compatible with the
kernels that are already released, and how the driver chooses among
installed files after an incompatible change.

The unit of compatibility is the ABI epoch. An epoch is one version of the
interfaces between the driver and the firmware. For each chip, an epoch has
a complete set of firmware files with fixed names, and those files implement
that version of the interfaces. Within an epoch, every firmware release
works with every kernel that supports the epoch. A firmware change that
cannot meet that rule starts a new epoch, with a new set of filenames. The
files of the old epoch stay installed for the kernels that request them.

Epoch 0 is the only epoch, so the driver requests one set of filenames and
has nothing to choose between. The selection rules in this document bind the
driver from the first kernel release that supports a second epoch.

Compatibility
=============

The kernel and linux-firmware are released and updated independently, so a
system can pair any kernel with any firmware release.
:doc:`../../../driver-api/firmware/firmware-usage-guidelines` sets two rules
for that pairing. A user who installs a newer kernel must not have to install
newer firmware to keep the hardware working. A firmware update must not
break a kernel that is already released. For nova-core, that means that
every kernel that supports an epoch boots and operates the GPU with every
firmware release in that epoch, whichever of the two is newer.

A feature or a fix that needs both a kernel change and a firmware change
takes effect once both are installed. Until then, the kernel and the
firmware provide only what both of them support.

The guidelines also say how a firmware file is versioned. The file's name
carries its major version and no other part of the version. An update that
stays compatible replaces the file under the same name. An incompatible
change gets a new major version, and the kernel keeps loading the older
major versions. For nova-core firmware, the epoch number is that major
version, and an unsuffixed name carries epoch 0. Adding an epoch removes no
file and deprecates no epoch.

A security fix that needs both a kernel change and a firmware change is
applied to every epoch that a supported stable or longterm kernel requests.
The kernel change detects whether the installed firmware carries the fix, as
the guidelines require. When an epoch cannot be made safe, the guidelines
allow a kernel to stop loading it, as a last option that is stated clearly
in every communication about the fix. A system that runs that kernel needs
firmware from an epoch that the kernel still loads.

Directory layout
================

This document describes the layout under ``nvidia/gpu/<chip>/gsp/``, which
is the layout that the first linux-firmware submission uses. The driver in
this tree requests ``nvidia/<chip>/gsp/``, the directory that nouveau uses,
and the driver changes to the new location before that submission. No nova-core
firmware is in linux-firmware, so no released kernel loads nova-core
firmware from there, and the change breaks no released kernel. Any one
kernel release requests one of the two locations and never falls back to
the other.

linux-firmware installs nova-core's files below ``nvidia/gpu``, one directory
per chip. The directory is named with the lowercase chip name, and the GSP
images are in its ``gsp`` subdirectory::

    /lib/firmware/nvidia/gpu/<chip>/gsp/
        <image>.tlv
        <image>.bin

The driver requests the path ``nvidia/gpu/<chip>/gsp/<image>.tlv``, and the
firmware loader opens that path below ``/lib/firmware`` or below another
directory of its search path.

Every image has a ``.tlv`` file that holds the image's metadata in the
tag-length-value (TLV) format that :doc:`tlv` defines. The payload is either
inside the TLV file or in a ``.bin`` companion file that the TLV file names.
:doc:`tlv` also lists the images, the tags of each image, and which images
use a companion file.

nouveau requests its GSP-RM files from ``nvidia/<chip>/gsp/``, and that path
does not change. A nouveau filename carries the GSP-RM release number after
a hyphen, as in ``gsp-570.144.bin``. A nova-core epoch suffix has the same
shape. The two schemes produce no common filename, but the ``gpu`` path
component keeps them in separate directories, so that a number after a
hyphen means one thing within a directory.

When several chips use the same image, linux-firmware installs one copy and
symbolic links to it. A link can replace a chip directory, a ``gsp``
subdirectory, or a single file. linux-firmware already links nouveau's files
at each of those three levels.

ABI epochs
==========

For one chip, an epoch is the complete set of files with which the driver
boots the chip, together with every interface between the driver and the
firmware in those files:

- the TLV tags that the driver reads, which :doc:`tlv` documents
- the boot arguments that the driver passes to GSP-RM
- the framebuffer layout that the driver and GSP-RM both assume
- the command and message interface between the driver and GSP-RM once
  GSP-RM runs

Two tests decide whether a change stays within its epoch:

- A firmware release stays in its epoch only if every kernel that supports
  the epoch boots and operates the GPU with that release.
- A kernel supports an epoch only if the kernel boots and operates the GPU
  with every firmware release that is already published in that epoch.

A change that fails either test starts a new epoch. An addition passes both
tests when an older kernel can ignore it and a newer kernel works without
it.

nova-core defines the TLV tags, and :doc:`tlv` documents them. The driver
derives what GSP-RM needs from the tag values, and GSP-RM defines none of
the tags. So a change to the GSP-RM interface and a change to a tag are
separate events, and the two tests apply to each of them separately.
:doc:`tlv` states which tag changes stay within an epoch.

File names
==========

The files of epoch 0 have no suffix. A file of a later epoch has a hyphen
and the epoch number before its extension. A chip with epochs 0 and 1
installed has both sets in one directory::

    /lib/firmware/nvidia/gpu/<chip>/gsp/
        <image>.tlv
        <image>.bin
        <image>-1.tlv
        <image>-1.bin

Every chip's epoch numbers come from one sequence, and a number is never
reused. So an epoch number names the same version of the interfaces for
every chip, and one implementation of that version in the driver. A change
can affect some chips and not others, and an unaffected chip keeps the files
that it has, so the installed epochs of one chip can have gaps. A file set
is named for the epoch whose interfaces the firmware implements. So a chip
whose firmware is first submitted while epoch 2 is the newest epoch has
files named for epoch 2, and it has no unsuffixed files.

A new epoch adds files and changes none. Every file that the affected chip
needs gets the new suffix, including a file whose contents did not change,
so that each epoch's file set is complete on its own. An update to one epoch
then never alters another. The driver never uses the files of one epoch with
the files of another. A path that names an epoch reaches a file of that
epoch through every symbolic link on the way, so that ``<image>-1.bin``
never links to ``<image>.bin``.

When a TLV file keeps its payload in a companion file, its ``FILE`` tag
names that file. The TLV format lets the tag name any file in the directory,
and the driver requests the file that the tag names. For nova-core firmware,
the name is the TLV file's own basename with a ``.bin`` extension, so
``gsp-1.tlv`` names ``gsp-1.bin``. The rule makes the name predictable, so
that the module firmware metadata can list the companion file in advance.
`Module firmware metadata`_ says why the list has to name the file in
advance.

The ``VERS`` tag inside a TLV file names the firmware release, and no tag
names the epoch. The files of two epochs are installed side by side, so the
files need distinct names, and the driver has to know a file's name before
it can open the file and read its tags. So the epoch is in the filename.

Epoch selection
===============

While the driver supports one epoch, it requests that epoch's files and
fails the probe when any file is missing or malformed. The rules below apply
from the first kernel release that supports a second epoch.

For each chip, the driver has a list of the epochs that it supports. The
list is fixed when the driver is built, and it holds every epoch that exists
for the chip at that time, minus any epoch that was dropped under the
security exception in `Compatibility`_. The driver tries the listed epochs
from the highest number down. It never requests an epoch that is not on its
list, so that firmware from an epoch newer than any on the list has no
effect on the driver.

An epoch is installed when all of its files are present and parse: every TLV
file that the chip needs, and every companion file that those TLV files
name. To check an epoch, the driver requests and parses each TLV file of the
set and loads each companion file that a TLV file names. The driver finishes
the whole set before it uses any file. The check has one of four outcomes:

- Every TLV file is missing. The epoch is not installed, and the driver
  tries the next lower epoch on its list. Only a file-not-found error counts
  as missing, and a companion file whose TLV file is missing is never
  requested.
- Every file is present and parses. The driver selects the epoch.
- Some files are present and others are missing, or a file does not parse.
  The installation is broken, so the driver logs the file that is missing or
  malformed and the probe fails. Falling back to a lower epoch would hide
  the breakage, so the driver does not fall back.
- Any other error, such as a failed allocation, fails the probe.

The driver requests the TLV files with ``firmware_request_nowarn``, so that
a system with only epoch 0 installed logs nothing about the higher epochs
that the driver checked first. The driver logs the epoch that it selected.
When that epoch is lower than the highest one on its list, the message is a
warning, because the installed firmware lacks the changes that the higher
epochs carry. When no listed epoch is installed, the driver logs the epochs
that it tried and the probe fails.

After selection, the driver uses only files of the selected epoch. A failed
signature check or an image that the hardware rejects fails the probe, with
no fallback to a lower epoch.

The firmware loader offers no directory listing, so the driver learns what
is installed only by requesting names that it knows. If epochs 2 and 0 are
installed, a driver whose list holds 2, 1, and 0 selects epoch 2. With the
same files installed, a driver whose list holds 1 and 0 finds no epoch 1
file and selects epoch 0.

Module firmware metadata
========================

The module firmware metadata is the list of firmware filenames that the
module carries and that ``modinfo -F firmware`` prints. An initramfs
generator copies the files that the list names into the initramfs. When
nova-core probes from an initramfs, the firmware loader finds only the files
that the initramfs holds, so a file that is not on the list is missing there
even when it is installed on the root filesystem. The generator cannot know
which epoch the driver will find installed, so the list names every TLV file
and every companion file of every listed epoch, for every chip.

The companion naming rule in `File names`_ exists for this list. The driver
reads the ``FILE`` tag at run time, but the list is fixed when the module is
built. The rule fixes the companion file's name, so that the list can name
the file before any TLV file is read. If a ``FILE`` tag named any other
file, that file would be absent from the list and from every initramfs that
is built from the list, and the probe would fail there.

Cost of an epoch
================

A new epoch adds an implementation of its interfaces to the driver, a
complete file set for every affected chip to linux-firmware, and the
matching entries to the module firmware metadata. The implementation stays
in the driver for as long as the epoch is on the list, and nova-core testing
covers the implementation for that whole time.

linux-firmware keeps the files of every epoch, so each retained epoch adds
files or symbolic links to the installed tree. A distribution that puts
nova-core in its initramfs copies every file that the module firmware
metadata names, so each listed epoch adds files to every initramfs as well.
Because of these costs, a change that can stay within its epoch stays there.

Retiring an epoch
=================

An epoch stays in use by every kernel that lists it, including a kernel that
prefers a higher epoch. That kernel falls back to the older epoch when the
older epoch is the one that is installed. Retiring an epoch takes two steps
that can be years apart. A new kernel drops the epoch from its lists and
removes the implementation. linux-firmware removes the files once every
kernel that still requests them has reached end of life. An enterprise
kernel that requests an epoch keeps the files of that epoch in
linux-firmware for a decade or more.

Propose the removal on nova-gpu@lists.linux.dev, naming the epoch, the chips
that it covers, every kernel release that requests it, and each release's
end-of-life date.

Under the security exception in `Compatibility`_, a kernel may drop an epoch
without waiting. Removing the files from linux-firmware still waits for
every kernel that requests them to reach end of life.
