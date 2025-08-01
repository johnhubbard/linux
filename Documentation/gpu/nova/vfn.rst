VFN (virtual function notification) Interrupt Architecture
==========================================================
Modern Nvidia GPUs use a hierarchical interrupt architecture, with virtualization
support added recently. The "NV_CTRL" block is the main interrupt controller, which
is responsible for routing interrupts. Both MSI and legacy interrupt sources are
supported.

To support virtualization, all the interrupt sources are duplicated for each VF
including one set for the PF. Thus, each virtual function has the same "view" of the
interrupt source registers as the PF. This is where the "tree" architecture of the
VFN comes from. The top-level root has one top-level register per branch (leading to
a subtree) for each VF, and each VF has leaf registers corresponding to each interrupt
source.

Following is a diagram of the interrupt architecture to picture this::

                         Host CPU/OS
                              ^
                       [MSI-X or Legacy PCI interrupt Delivery]
                              ^
    +---------------------------------------------------------------------+
    |                          NV_CTRL                                    |
    |                    Main Interrupt Controller                        |
    |                                                                     |
    |  +---------------------------------------------------------------+  |
    |  |               CPU_INTR_TOP[0..63]                             |  |
    |  |              (64 Top-Level Registers)                         |  |
    |  |                                                               |  |
    |  | [0]     [1]     [2]     [3]  ...  [62]    [63]                |  |
    |  | PF      VF1     VF2     VF3        VF62    VF63               |  |
    |  | |       |       |       |    ...   |      |                   |  |
    |  +-+-------+-------+-------+----------+------+-------------------+  |
    |    |       |       |       |          |      |                      |
    |    |       |       |       |    ...   |      |                      |
    +----+-------+-------+-------+----------+------+----------------------+
         |       |       |       |          |      |
         v       v       v       v          v      v
    +---------------------------------------------------------------------+
    │                  CPU_INTR_LEAF[0..1023]                             │
    │                 (1024 Leaf Registers Total)                         │
    │                                                                     │
    │  PF (TOP[0]) Leaves:        VF1 (TOP[1]) Leaves:                    │
    │  +---------------------+     +---------------------+                │
    │  │ Leaf[0]:  PFIFO     │     │ Leaf[16]: PFIFO     │                │
    │  │ Leaf[1]:  Graphics  │     │ Leaf[17]: Graphics  │                │
    │  │ Leaf[2]:  Compute   │     │ Leaf[18]: Compute   │                │
    │  │ Leaf[3]:  Copy Eng  │     │ Leaf[19]: Copy Eng  │                │
    │  │ Leaf[4]:  Display   │     │ Leaf[20]: Display   │                │
    │  │ Leaf[5]:  Memory    │     │ Leaf[21]: Memory    │                │
    │  │ Leaf[6]:  Timer     │     │ Leaf[22]: Timer     │                │
    │  │ Leaf[7]:  Power Mgmt│     │ Leaf[23]: Power Mgmt│                │
    │  │ Leaf[8]:  Thermal   │     │ Leaf[24]: Thermal   │                │
    │  │ Leaf[9]:  Bus/PCIe  │     │ Leaf[25]: Bus/PCIe  │                │
    │  │ Leaf[10]: NVENC     │     │ Leaf[26]: NVENC     │                │
    │  │ Leaf[11]: NVDEC     │     │ Leaf[27]: NVDEC     │                │
    │  │ Leaf[12]: GSP       │     │ Leaf[28]: GSP       │                │
    |  | Leaf[13]: Security  |     | Leaf[29]: Security  |                |
    |  | Leaf[14]: Custom    |     | Leaf[30]: Custom    |                |
    |  | Leaf[15]: Debug     |     | Leaf[31]: Debug     |                |
    |  +---------------------+     +---------------------+                |
    |         ^                           ^                               |
    |    Each register                Each register                       |
    |    has 32 IRQ bits              has 32 IRQ bits                     |
    |                                                                     |
    |  VF2 (TOP[2]) Leaves:        ...    VF63 (TOP[63]) Leaves:          |
    |  +---------------------+             +---------------------+        |
    |  | Leaf[32]: PFIFO     |             | Leaf[1008]: PFIFO   |        |
    |  | Leaf[33]: Graphics  |             | Leaf[1009]: Graphics|        |
    |  | Leaf[34]: Compute   |             | Leaf[1010]: Compute |        |
    |  | Leaf[35]: Copy Eng  |      ...    | Leaf[1011]: Copy Eng|        |
    |  | Leaf[36]: Display   |             | Leaf[1012]: Display |        |
    |  | Leaf[37]: Memory    |             | Leaf[1013]: Memory  |        |
    |  | Leaf[38]: Timer     |             | Leaf[1014]: Timer   |        |
    |  | Leaf[39]: Power Mgmt|             | Leaf[1015]: Power Mgmt       |
    |  | Leaf[40]: Thermal   |             | Leaf[1016]: Thermal |        |
    |  | Leaf[41]: Bus/PCIe  |             | Leaf[1017]: Bus/PCIe|        |
    |  | Leaf[42]: NVENC     |             | Leaf[1018]: NVENC   |        |
    |  | Leaf[43]: NVDEC     |             | Leaf[1019]: NVDEC   |        |
    |  | Leaf[44]: GSP       |             | Leaf[1020]: GSP     |        |
    |  | Leaf[45]: Security  |             | Leaf[1021]: Security|        |
    |  | Leaf[46]: Custom    |             | Leaf[1022]: Custom  |        |
    |  | Leaf[47]: Debug     |             | Leaf[1023]: Debug   |        |
    |  +---------------------+             +---------------------+        |
    +---------------------------------------------------------------------+

Formula: VF(f) uses Leaves[16*f ... 16*f+15]
- PF  (f=0):  Leaves[0..15]
- VF1 (f=1):  Leaves[16..31]
- VF2 (f=2):  Leaves[32..47]
- VF3 (f=3):  Leaves[48..63]
- ...
- VF63(f=63): Leaves[1008..1023]

Each Leaf Register = 32-bit interrupt bits for a specific interrupt source (PFIFO, Graphics, etc.)

Total System Capacity:
- 64 VFs maximum (Top level, also known as GFID. Example, PF is GFID=0, VF1 is GFID=1, VF2 is GFID=2, etc.)
- 16 interrupt sources corresponding to each top-level - also known as LEAFs.
- Each leaf corresponds to a specific interrupt source (PFIFO, Graphics, etc.)
- 512 interrupt sources per VF (16 leaves × 32 bits each)
- 32,768 total interrupt sources (64 × 512)

VFN Register Mapping Formula for offset from VFN base (for VF with GFID=f):
VFN_INTR_TOP_STATUS   CPU_INTR_TOP[f] = (0x1600 + f*4);   /* 4 bytes per top-level register */
VFN_INTR_RESET_BASE   CPU_INTR_LEAF[16*f + 0..15] = (0x1000 + f*64) /* 16 regs per leaf, each 4 bytes */
VFN_INTR_ALLOW_BASE   CPU_INTR_LEAF_EN_SET[16*f + 0..15] = (0x1200 + f*64) /* 16 regs per leaf, each 4 bytes */
VFN_INTR_BLOCK_BASE   CPU_INTR_LEAF_EN_CLEAR[16*f + 0..15] = (0x1400 + f*64) /* 16 regs per leaf, each 4 bytes */

Note that the GFID for PF is always 0.