.. SPDX-License-Identifier: (GPL-2.0+ OR MIT)

=============================
Firmware files and ABI epochs
=============================

Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.

The nova-core driver loads GSP-RM and its boot images from linux-firmware.
Firmware releases within one ABI epoch use stable filenames and remain
compatible with every kernel that supports that epoch. An incompatible
release uses a new set of filenames identified by a new epoch.

Compatibility
=============

The kernel and linux-firmware are distributed and updated separately. Within
an epoch, every kernel release must work with every firmware release. A
kernel-firmware combination provides only the features and fixes that both
releases support.

:doc:`../../../driver-api/firmware/firmware-usage-guidelines` states the
kernel-wide rules for firmware distributed through linux-firmware:

- A newer major version of a file stays compatible with every kernel that
  loads that major version.
- A new major version requires the kernel to keep loading the older major
  versions.

For these files, the epoch number is the major version described by the
kernel-wide guidelines. Adding an epoch does not by itself deprecate an older
epoch.

A security fix should update every epoch retained by supported kernels. If
an older epoch cannot be fixed and continuing to load it is unsafe, a new
kernel may omit that epoch from its supported set. Omitting an epoch for this
reason requires both the kernel and firmware to be updated and is a last
resort under the kernel-wide guidelines.

Directory layout
================

Firmware submitted for nova-core is installed in a per-chip directory below
``nvidia/gpu``. The chip directory uses the lowercase chip name::

    /lib/firmware/nvidia/gpu/<chip>/gsp/
        <image>.tlv
        <image>.bin

The driver requests these paths relative to the firmware search path.
nova-core requests ``nvidia/<chip>/gsp/`` today and moves to ``nvidia/gpu``
in the change that submits nova-core firmware to linux-firmware. It requests
one of the two locations, never both, and never falls back from one to the
other.

See :doc:`tlv` for the image names, their TLV metadata, and companion payload
files.

linux-firmware may use a symbolic link when several chips use the same
image. The chip directory, the ``gsp`` subdirectory, or an individual file
may be a symbolic link. A linked file for one epoch resolves only to a file
from the same epoch.

nouveau requests its own GSP-RM files below ``nvidia/<chip>/gsp/`` and keeps
that location. Installing nova-core firmware below ``nvidia/gpu`` keeps the
two sets in separate directories.

ABI epochs
==========

An epoch covers the complete set of files needed to boot a chip and the
interfaces between those files and the driver. A new firmware release stays
within an epoch only if every existing kernel that supports the epoch can
boot and operate with it. A new kernel may claim support for an epoch only
if it can use every firmware release already published in that epoch. A
change that violates either requirement needs a new epoch.

The driver defines the TLV format as packaging metadata. GSP-RM does not
define these tags, and they are separate from its command and message
interface. Within each firmware image type, a published tag keeps its value
type and meaning permanently. A deprecated tag remains reserved and is never
assigned a new meaning.

Adding an optional tag does not require a new epoch because an older parser
ignores it. Removing a tag required by an older driver, or making a new tag
mandatory without a fallback for older firmware, violates the same-epoch
compatibility rule. See :doc:`tlv` for the tag compatibility rules.

The bidirectional compatibility rule also applies to the GSP-RM command and
message interface, boot parameters, startup arguments, and framebuffer
ownership. A compatible extension stays in the existing epoch. A change
requires a new epoch if it prevents an older supporting kernel from operating
with new firmware or a new supporting kernel from operating with older
firmware.

File names
==========

Files without a numeric suffix belong to epoch 0, the only epoch that exists
today. Every filename for a later epoch includes a dash and the epoch number
before the extension. Epoch numbers increase and are never reused::

    /lib/firmware/nvidia/gpu/gb202/gsp/
        <image>.tlv
        <image>.bin
        <image>-1.tlv
        <image>-1.bin

Adding an epoch does not modify or remove a file from an older epoch. Every
file required for an affected chip receives the new suffix, including files
whose contents did not change. Files from different epochs must not be
combined.

A TLV that uses a companion payload names the conventional basename for the
same epoch in its ``FILE`` tag. For example, ``gsp-1.tlv`` names
``gsp-1.bin``. A distributed TLV file must use the conventional companion
basename because the driver must know every candidate path during epoch
selection and when the module firmware metadata is generated.

The ``VERS`` tag inside a TLV names the firmware release, not the epoch. The
epoch is in the filename because the driver has to choose between installed
files before it can open one.

Epoch selection
===============

Only epoch 0 exists today, so nova-core has one supported epoch and requests
the unsuffixed names directly. The rules below apply once the driver supports
more than one epoch.

For each chip, the driver keeps an explicit list of supported epochs in
descending preference order. The list may omit lower-numbered epochs. The
driver ignores an installed epoch that is absent from this list.

The driver checks each supported epoch in preference order. Each candidate
consists of every TLV file and companion payload required for the chip. The
driver requests the complete set, parses the TLV files, and loads the
companions. The result is one of the following:

- If every required file is absent, the epoch is not installed and the
  driver checks the next supported epoch.
- If every required file is present and valid, the driver selects the epoch
  and uses only that file set for the boot.
- If only part of the set is present, or any file is malformed, the probe
  fails without falling back to an older epoch.
- Any other request, validation, or allocation error also fails the probe
  without fallback.

Probing an epoch that may be absent uses ``firmware_request_nowarn``, so a
system with only epoch 0 installed logs nothing for the higher epochs the
driver tried first.

A signature verification or hardware acceptance failure after epoch
selection also fails the probe without fallback. Fallback occurs only when
every required file for a candidate is absent. This prevents a partial
installation or rejected image from silently selecting an older epoch.

The driver requests firmware images at several points across probe and boot.
Epoch selection completes before the first image is used, so the epoch cannot
be inferred from one image after others have already been consumed.

The firmware loader does not provide a directory listing. The driver probes
only the filenames for epochs it supports. For example, if epochs 2 and 0
are installed, a driver that supports 2, 1, and 0 selects epoch 2. A driver
that supports only 1 and 0 ignores epoch 2 and selects epoch 0 after finding
all epoch 1 files absent.

The driver logs the epoch it selected. When that epoch is lower than the
highest one it supports, the message is a warning, because the installed
firmware lacks the changes that each newer epoch carries.

Module firmware metadata
========================

For every supported epoch, the ``MODULE_FIRMWARE`` entries explicitly name
every TLV file and companion payload that the driver may request. The
supported epochs and conventional basenames are fixed when the driver is
built, but the driver selects an epoch at runtime from the firmware files
installed on the system. Listing every candidate set allows an initramfs
generator to include all files the driver may select.

The value of a ``FILE`` tag must match the companion basename listed for its
epoch. A runtime-selected companion name cannot be represented in the static
module firmware metadata.

Cost of an epoch
================

The driver maintains an ABI implementation for every epoch it supports, and
each implementation requires testing. Supporting a new epoch requires a
driver implementation, a complete file set in linux-firmware, and matching
module firmware metadata.

linux-firmware retains files for epochs used by supported kernels. Each
retained epoch adds files or symbolic links to the installed firmware tree.
Symbolic links limit duplicate payloads when several chips use the same
image.

A distribution that includes nova-core in the initramfs copies the files
named in the module metadata. Each epoch supported by that kernel can
increase the initramfs size.

Retiring an epoch
=================

An epoch remains usable by every kernel whose supported list contains it,
even when that kernel prefers a higher epoch. Its files stay in
linux-firmware while any supported kernel may request them. An epoch becomes
eligible for normal removal after every such kernel has reached end of life.
Support lifetimes for enterprise kernels may require linux-firmware to
retain an epoch for a decade or more.

A proposal to retire an epoch is sent to nova-gpu@lists.linux.dev and
identifies the epoch, every kernel release that needs it, and each release's
end-of-life date. Coordinated changes remove the driver implementation and
the files from linux-firmware.

A security-driven deprecation follows the exception described under
`Compatibility`_ instead of the normal end-of-life schedule.
