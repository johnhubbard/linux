.. SPDX-License-Identifier: (GPL-2.0+ OR MIT)

==================================
TLV Tags in Nova Firmware Images
==================================

Nova firmware images use a Type-Length-Value (TLV) format to encapsulate
firmware components and metadata. The TLV file begins with a 4-byte "magic"
header that contains the string "NVFW".  Following the header is a sequence of
TLV blocks.

Each block consists of a 4-byte tag of ASCII characters, a 4-byte length
encoded as a little-endian unsigned integer, and a sequence of bytes, the size
of which is equal to the length rounded up to the next multiple of 4.

The driver code that reads the TLV and uses its contents is called the parser.
It is the responsibility of the parser to handle missing or malformed tags,
lengths, and values in the TLV.

::

    +------+------+------+------+
    |  'N' |  'V' |  'F' |  'W' |  Magic header
    +------+------+------+------+
    |  Tag (4 bytes, ASCII)     |  TLV block 0
    +---------------------------+
    |  Length (4 bytes, LE)     |
    +---------------------------+
    |                           |
    |  Value (length bytes,     |
    |  padded to 4-byte align)  |
    |                           |
    +---------------------------+
    |  Tag (4 bytes, ASCII)     |  TLV block 1
    +---------------------------+
    |  Length (4 bytes, LE)     |
    +---------------------------+
    |                           |
    |  Value (length bytes,     |
    |  padded to 4-byte align)  |
    |                           |
    +---------------------------+
    |         ...               |  More TLV blocks
    +---------------------------+

Firmware Image Files
====================

nova-core loads one TLV file per firmware image. The basename of the file
names the image, and each image has its own section of tags below:

``gsp.tlv``
    The GSP-RM image, with the tags under `GSP Firmware Tags`_. Its payload
    is in the companion file ``gsp.bin``.

``gsp_bootloader.tlv``
    The bootloader that loads GSP-RM, with the tags under
    `GSP Bootloader Tags`_.

``booter_load.tlv`` and ``booter_unload.tlv``
    The SEC2 Booter images that load and unload GSP-RM on Turing, Ampere,
    and Ada, with the tags under `Booter Firmware Tags`_.

``gen_bootloader.tlv``
    The falcon bootloader that loads FWSEC (:doc:`fwsec`) on Turing and
    GA100, with the tags under `Generic Bootloader Tags`_.

``fmc.tlv``
    The GSP-FMC image that FSP (:doc:`fsp`) loads on Hopper and later, with
    the tags under `FMC Firmware Tags`_.

Every chip uses ``gsp.tlv`` and ``gsp_bootloader.tlv``. The other images
depend on how the chip boots the GSP, and the module firmware metadata,
which :doc:`firmware` describes, lists the complete set for each chip.

A TLV file either carries its payload in a ``BLOB`` tag or names a companion
file with a ``FILE`` and ``SIZE`` pair, as described under `Common Tags`_. A
companion file has the same basename as its TLV file and a ``.bin``
extension. :doc:`firmware` states where the files are installed and how
their names change across ABI epochs.

Tags and Length
===============
TLV tags are always four-character words, with all letters being upper case.
Duplicate tags are not allowed.

A TLV file may contain additional tags not described in this document.

Tag Compatibility
=================

Once a tag has shipped in linux-firmware for a firmware type, its value type
and its meaning for that firmware type are fixed. To carry different data,
add a new tag and mark the old one deprecated in this document. A deprecated
tag name is never reused.

A parser ignores a tag that it does not recognize, so adding a tag does not
affect an older kernel. A newer kernel that reads the new tag must still
work when the tag is absent, because firmware that was published earlier in
the same ABI epoch lacks the tag. When the kernel cannot work without the
tag, the tag needs a new epoch. Removing a tag that an existing kernel
requires also needs a new epoch. :doc:`firmware` defines epochs.

Values
======
Values are one of four types.  The type is not encoded in the format; rather,
the parser expects a given tag to have a value of a given type.

1) Integers, encoded in 32-bit or 64-bit little-endian format.
2) Strings, encoded as-is and required to be only printable ASCII characters
   and without a null terminator.
3) An array of bytes, for binary data.
4) Boolean, encoded as single byte, with a value of 0 for False or 1 for True.

Common Tags
===========
These tags are shared across firmware types and carry the same meaning
wherever they appear.  Unlike the firmware-specific tags below, a common tag
is reserved: its meaning is fixed and may never be redefined for a particular
firmware type.

``VERS`` (string)
    Human-readable firmware version string.  Present in all TLV files.

A TLV image must contain either a single ``BLOB`` tag (firmware embedded
inline) or a ``SIZE``/``FILE`` pair (firmware stored in a separate file).

``BLOB`` (bytes)
    If the firmware microcode binary is stored in the TLV, this tag contains
    the actual firmware image bytes.

``FILE`` (string)
    If the firmware binary is stored as a separate file, this tag contains the
    name of that file, which is required to be in the same directory as the TLV,
    so no paths are allowed in the filename.  This tag is always paired with
    ``SIZE``, so as to allow the driver to pre-allocate the buffer before
    loading the file.

``SIZE`` (u32)
    Total size in bytes of the firmware image to be loaded from the companion
    file named by ``FILE``.  This tag is mandatory if ``FILE`` exists, so the
    size of the firmware image must be known when the TLV is created.  If the
    firmware image is updated and its size changes, then the TLV must be
    updated with it.

GSP Firmware Tags
=================
``SIGN`` (bytes)
    Cryptographic signature for the GSP firmware.

``BLID`` (string)
    The build ID, extracted from the ".note.gnu.build-id" section.

Booter Firmware Tags
====================
``DAOF`` (u32) - ``os_data_offset``
    OS data section offset within the firmware image (absolute byte offset).
    Maps to the DMEM load source.

``DASZ`` (u32) - ``os_data_size``
    OS data section size in bytes.

``CDOF`` (u32) - ``os_code_offset``
    OS code section offset within the firmware image (absolute byte offset).
    Maps to the non-secure IMEM load source.

``CDSZ`` (u32) - ``os_code_size``
    OS code section size in bytes.

``PLOC`` (u32) - ``patch_loc``
    Signature patch location -- byte offset within the firmware image where the
    selected signature should be written.

``FUSE`` (u32) - ``fuse_version``
    Fuse version of the firmware, used with the hardware fuse register to
    select the correct signature index.

``ENID`` (u32) - ``engine_id``
    Engine ID mask identifying the falcon engine this firmware targets.

``UCID`` (u32) - ``ucode_id``
    Microcode ID used together with the engine ID to query hardware signature
    fuse registers.

``A0CO`` (u32) - ``app0_code_offset``
    App0 code offset -- start of the secure code region within the firmware
    image. Used as the IMEM secure section source.

``A0CS`` (u32) - ``app0_code_size``
    App0 code size in bytes.

``NSIG`` (u32) - ``num_sigs``
    Number of signatures included in the ``SIGN`` tag.

``SIGN`` (bytes)
    Concatenated array of firmware signatures. The size of each signature is
    the total length of the ``SIGN`` value divided by ``NSIG``. The correct
    signature is selected using the fuse-version-derived index.

Generic Bootloader Tags
=======================
``CDSZ`` (u32) - ``code_size``
    Size in bytes of the bootloader code to copy from the ``BLOB`` tag and
    PIO-load into falcon IMEM.

``STRT`` (u32) - ``start_tag``
    Start tag identifying the IMEM block where execution begins.  The falcon
    boot address is derived as ``start_tag << 8``.

GSP Bootloader Tags
===================
``CDOF`` (u32) - ``code_offset``
    Offset within the firmware image at which the code section starts.

``DAOF`` (u32) - ``data_offset``
    Offset within the firmware image at which the data section starts.

``MFOF`` (u32) - ``manifest_offset``
    Offset within the firmware image at which the manifest starts.

``APPV`` (u32) - ``app_version``
    Application version of the firmware.

FMC Firmware Tags
=================
``HASH`` (bytes)
    SHA-384 hash of the FMC firmware, exactly 48 bytes long.

``PKEY`` (bytes)
    Public key used to verify the FMC firmware. At most 384 bytes (RSA-3072),
    but may be shorter.

``SIGN`` (bytes)
    Signature of the FMC firmware. At most 384 bytes (RSA-3072), but may
    be shorter.
