#![allow(non_camel_case_types)]

use typestate::bitregs;

bitregs! {
    /// ID_AA64MMFR0_EL1 — AArch64 Memory Model Feature Register 0
    /// Purpose:
    ///     Provides information about the implemented memory model and memory management support in AArch64 state
    /// # Safety
    ///     all field is ReadOnly
    pub(crate) struct ID_AA64MMFR0_EL1: u64 {
        // Physical Address range supported (PA size).
        //   0b0000=32b/4GB, 0001=36b/64GB, 0010=40b/1TB, 0011=42b/4TB,
        //   0100=44b/16TB, 0101=48b/256TB, 0110=52b/4PB (when FEAT_LPA),
        //   0111=56b/64PB (when FEAT_D128). Others: reserved.
        pub(crate) parange@[3:0] as PARange {
            PA32bits4GB = 0b000,
            PA36bits64GB = 0b001,
            PA40bits1TB = 0b010,
            PA42bits4TB = 0b011,
            PA44bits16TB = 0b100,
            PA48bits256TB = 0b101,
            PA52bits4PB = 0b110,
            PA56bits64PB = 0b111,
        },

        // Number of ASID bits:
        //   0b0000 = 8-bit ASID
        //   0b0010 = 16-bit ASID
        //   others = reserved
        pub(crate) asidbits@[7:4],

        // Mixed-endian support at EL1/EL2/EL3:
        //   0b0000 = No mixed-endian (SCTLR_ELx.EE fixed)
        //   0b0001 = Mixed-endian supported (SCTLR_ELx.EE configurable)
        //   others = reserved
        pub(crate) bigend@[11:8],

        // Distinction between Secure and Non-secure Memory:
        //   0b0000 = Not supported (not permitted if EL3 is implemented)
        //   0b0001 = Supported
        //   others = reserved
        pub(crate) snsmem@[15:12],

        // Mixed-endian support at EL0 only:
        //   0b0000 = No mixed-endian at EL0 (SCTLR_EL1.E0E fixed)
        //   0b0001 = Mixed-endian at EL0 supported (SCTLR_EL1.E0E configurable)
        //   others = reserved
        // Note: If BigEnd != 0b0000, this field is RES0/invalid.
        pub(crate) bigendel0@[19:16],

        // 16KB granule (stage 1) support:
        //   0b0000 = Not supported
        //   0b0001 = Supported
        //   0b0010 = Supported with 52-bit input/output (when FEAT_LPA2)
        //   others = reserved
        pub(crate) tgran16@[23:20],

        // 64KB granule (stage 1) support:
        //   0b0000 = Supported
        //   0b1111 = Not supported
        //   others = reserved
        pub(crate) tgran64@[27:24],

        // 4KB granule (stage 1) support:
        //   0b0000 = Supported
        //   0b0001 = Supported with 52-bit input/output (when FEAT_LPA2)
        //   0b1111 = Not supported
        //   others = reserved
        pub(crate) tgran4@[31:28],

        // 16KB granule at stage 2 (alternative ID scheme):
        //   0b0000 = See TGran16 (stage 1 field)
        //   0b0001 = Not supported at stage 2
        //   0b0010 = Supported at stage 2
        //   0b0011 = Supported with 52-bit input/output (when FEAT_LPA2)
        //   others = reserved
        // If EL2 not implemented: reads 0b0000. 0b0000 is deprecated when EL2 is implemented.
        pub(crate) tgran16_2@[35:32],

        // 64KB granule at stage 2 (alternative ID scheme):
        //   0b0000 = See TGran64 (stage 1 field)
        //   0b0001 = Not supported at stage 2
        //   0b0010 = Supported at stage 2
        //   others = reserved
        // If EL2 not implemented: reads 0b0000. 0b0000 is deprecated when EL2 is implemented.
        pub(crate) tgran64_2@[39:36],

        // Indicates support for 4KiB memory granule size at stage2
        // If EL2 is not implemented: res0
        pub(crate) tgran4_2@[43:40] as TGran4_2 {
            // Support for 4KB granule at stage 2 is identified in the
            // ID_AA64MMFR0_EL1.TGran4 field.
            SeeEL1 = 0b00,
            NotSupported = 0b01,
            Supported = 0b10,
            // 4KB granule at stage 2 supports 52-bit input addresses and can
            // describe 52-bit output addresses.
            // Applies when FEAT_LPA2 is implemented
            Supported52bit = 0b11,
        },

        // ExS — non-context-synchronizing exception entry/exit:
        //   0b0000 = All exception entries/exits are context-synchronizing
        //   0b0001 = Non-context-synchronizing entry/exit supported (FEAT_ExS)
        //   others = reserved
        pub(crate) exs@[47:44],

        reserved@[55:48] [res0],

        // FGT — Fine-Grained Trap controls presence:
        //   0b0000 = Not implemented (not permitted from Armv8.6)
        //   0b0001 = FEAT_FGT (first level of fine-grained traps)
        //   0b0010 = FEAT_FGT2 (extended fine-grained traps)
        //   others = reserved (from Armv8.9, 0b0001 is not permitted)
        pub(crate) fgt@[59:56],

        // ECV — Enhanced Counter Virtualization:
        //   0b00 = Not implemented
        //   0b01 = FEAT_ECV (counter views, EVNTIS, extends PMSCR/TRFCR fields)
        //   0b10 = FEAT_ECV_POFF (adds CNTPOFF_EL2 and control bits)
        pub(crate) ecv@[63:60] as ECV {
            NotImplemented = 0b00,
            // Enhanced Counter Virtualization is implemented. Supports CNTHCTL_EL2.{EL1TVT, EL1TVCT,
            // EL1NVPCT, EL1NVVCT, EVNTIS}, CNTKCTL_EL1.EVNTIS, CNTPCTSS_EL0 counter views,
            // and CNTVCTSS_EL0 counter views. Extends the PMSCR_EL1.PCT, PMSCR_EL2.PCT,
            // TRFCR_EL1.TS, and TRFCR_EL2.TS fields
            Implemented1 = 0b01,
            // As 0b0001, and the CNTPOFF_EL2 register and the CNTHCTL_EL2.ECV and SCR_EL3.ECVEn
            // fields are implemented.
            Implemented2 = 0b10,
        },
    }
}

bitregs! {
    /// VTCR_EL2 Virtualization Translation Control Register
    /// Purpose:
    ///     The control register for stage 2 of the EL1&0 translation regime.
    pub(crate) struct VTCR_EL2: u64 {
        // The size offset of the memory region addressed by VTTBR_EL2.
        // Region size = 2^(64 - T0SZ) bytes.
        // Permissible T0SZ depends on SL0 (and SL2 when DS==1) and the translation granule size (TG0).
        // If inconsistent with SL0/TG0, a stage-2 level-0 Translation fault is generated.
        pub(crate) t0sz@[5:0],

        // Stage-2 initial lookup level selector.
        // Encodings depend on TG0 and, when DS==1, the combination {SL2, SL0}.
        // If inconsistent with T0SZ/TG0, stage-2 Translation fault.
        pub(crate) sl0@[7:6],

        // Inner cacheability for stage-2 table walks:
        //   0b00 = Inner Non-cacheable
        //   0b01 = Inner WB RA WA Cacheable
        //   0b10 = Inner WT RA nWA Cacheable
        //   0b11 = Inner WB RA nWA Cacheable
        pub(crate) irgn0@[9:8],

        // Outer cacheability for stage-2 table walks:
        //   0b00 = Outer Non-cacheable
        //   0b01 = Outer WB RA WA Cacheable
        //   0b10 = Outer WT RA nWA Cacheable
        //   0b11 = Outer WB RA nWA Cacheable
        pub(crate) orgn0@[11:10],

        // Shareability for stage-2 table walks:
        //   0b00 = Non-shareable
        //   0b10 = Outer Shareable
        //   0b11 = Inner Shareable
        //   0b01 = Reserved
        pub(crate) sh0@[13:12],

        // Translation granule for stage 2:
        //   0b00 = 4KB, 0b01 = 64KB, 0b10 = 16KB, 0b11 = Reserved
        pub(crate) tg0@[15:14],

        // Output Physical Address Size of stage-2 translation:
        //   0b000=32b, 0b001=36b, 0b010=40b, 0b011=42b, 0b100=44b, 0b101=48b,
        //   0b110=52b when LPA2 semantics apply (DS==1); otherwise behaves as 48b.
        //   0b111=Reserved (do not program unless documented by the implementation).
        pub(crate) ps@[18:16],

        // VMID size control:
        //   0b0 = 8-bit VMID
        //   0b1 = 16-bit VMID (when FEAT_VMID16 is implemented)
        pub(crate) vs@[19:19],
        reserved@[20:20] [res0],

        // Hardware Access flag update (stage 2), when FEAT_HAFDBS is implemented:
        //   0b0=Disabled, 0b1=Enabled
        pub(crate) ha@[21:21],

        // Hardware Dirty state tracking (stage 2), when FEAT_HAFDBS is implemented:
        //   0b0=Disabled, 0b1=Enabled
        pub(crate) hd@[22:22],
        reserved@[24:23] [res0],

        // Hardware use of descriptor bit[59] for stage-2 Block/Page entries (IMPLEMENTATION DEFINED).
        // If not implemented, behaves as RES0/RAZ-WI per implementation.
        pub(crate) hwu59@[25:25],
        // Hardware use of descriptor bit[60] (IMPLEMENTATION DEFINED).
        pub(crate) hwu60@[26:26],
        // Hardware use of descriptor bit[61] (IMPLEMENTATION DEFINED).
        pub(crate) hwu61@[27:27],
        // Hardware use of descriptor bit[62] (IMPLEMENTATION DEFINED).
        pub(crate) hwu62@[28:28],

        // Address space for stage-2 table walks of Non-secure IPA:
        //   0b0 = Walks use Secure PA space
        //   0b1 = Walks use Non-secure PA space
        pub(crate) nsw@[29:29],

        // Address space for stage-2 output of Non-secure IPA:
        //   0b0 = Output PA is in Secure space
        //   0b1 = Output PA is in Non-secure space
        pub(crate) nsa@[30:30],
        reserved@[31:31] [res1],

        // LPA2 semantics enable for stage 2 (affects minimum T0SZ, descriptor formats, PS==0b110 meaning, and SL2 usage):
        //   0b0 = VMSAv8-64 semantics
        //   0b1 = Enable VMSAv8-64 with LPA2 semantics
        pub(crate) ds@[32:32],

        // Extra starting-level bit used together with SL0 when DS==1 (granule-specific; typically 4KB):
        //   When DS==0: RES0
        pub(crate) sl2@[33:33],

        // When FEAT_THE is implemented (default: unknown value)
        //  AssuredOnly attribute enable for VMSAv8-64. Configures use of bit[58] of the stage 2 translation table
        //  Block or Page descriptor.
        //    - 0b0: Bit[58] of each stage 2 translation Block or Page descriptor does
        //      not indicate AssuredOnly attribute
        //    - 0b1: Bit[58] of each stage 2 translation Block or Page descriptor
        //      indicate AssuredOnly attribute
        // When VTCR_EL2.D128 is set: res0
        // otherwise res0
        pub(crate) assured_only@[34:34],

        // When FEAT_THE is implemented (default: unknown value)
        //  Control bit to check for presence of MMU TopLevel1 permission attribute
        //    - 0b0: This bit does not have any effect on stage 2 translations
        //    - 0b1: Enables MMU TopLevel1 permission attribute check for TTBR0_EL1 and TTBR1_EL1 translations
        // otherwise res0
        pub(crate) tl1@[35:35],

        // When FEAT_THE is implemented (default: unknown value)
        //  Control bit to select the stage-2 permission model
        //    - 0b0: Direct permission model
        //    - 0b1: Indirect permission model
        // When VTCR_EL2.D128 is set: res1
        // otherwise: res0
        pub(crate) s2pie@[36:36],

        // When FEAT_S2POE is implemented (default: unknown value)
        //  Permission Overlay enable (stage 2). Not permitted to be cached in a TLB.
        //    - 0b0: Overlay disabled
        //    - 0b1: Overlay enabled
        // otherwise: res0
        pub(crate) s2poe@[37:37],

        // When FEAT_D128 is implemented (default: unknown value)
        //  Selects VMSAv9-128 translation system for stage 2:
        //    - 0b0: Follow VMSAv8-64 translation process
        //    - 0b1: Follow VMSAv9-128 translation process
        // otherwise: res0
        pub(crate) d128@[38:38],

        reserved@[39:39] [res0],

        // When FEAT_THE & FEAT_GCS are implemented (default: unknown value)
        //  Assured stage-1 translations for Guarded Control Stacks:
        //    - 0b0: AssuredOnly in stage 2 not required for privileged GCS data accesses
        //    - 0b1: AssuredOnly in stage 2 required for privileged GCS data accesses
        // otherwise: res0
        pub(crate) gcsh@[40:40],

        // When FEAT_THE is implemented (default: unknown value)
        //  Check for TopLevel0 permission attribute:
        //    - 0b0: No effect on stage-2 translations
        //    - 0b1: Enable TL0 attribute check for TTBR0_EL1/TTBR1_EL1 translations
        // otherwise: res0
        pub(crate) tl0@[41:41],

        reserved@[43:42] [res0],

        // When FEAT_HAFT is implemented (default: unknown value)
        //  Hardware-managed Access Flag for Table descriptors:
        //    - 0b0: Disabled
        //    - 0b1: Enabled
        // otherwise: res0
        pub(crate) haft@[44:44],

        // When FEAT_HDBSS is implemented (default: unknown value)
        //  Hardware tracking of Dirty state Structure:
        //    - 0b0: Disabled
        //    - 0b1: Enabled
        // otherwise: res0
        pub(crate) hdbss@[45:45],
        reserved@[63:46] [res0],
    }
}

bitregs! {
    /// VTTBR_EL2 — Virtualization Translation Table Base Register
    /// # Safety
    ///     Unsupported when VTTBR_EL2 is 128-bit.
    ///     When FEAT_D128 is implemented and VTCR_EL2.D128 == 1, VTTBR_EL2 becomes 128-bit.
    pub(crate) struct VTTBR_EL2: u64 {
        // CnP — Common not Private:
        //   0b0 = Translation table pointed to by this VTTBR is private to the PE.
        //   0b1 = Translation table entries are common across PEs in the same Inner Shareable domain.
        //         Using different tables with the same VMID while CnP==1 is CONSTRAINED UNPREDICTABLE.
        pub(crate) cnp@[0:0],

        // SKL — Skip Level:
        //   Determines how many levels to skip from the regular start level of the
        //   Non-secure stage-2 translation table walk.
        //     0b00 = Skip 0 level
        //     0b01 = Skip 1 level
        //     0b10 = Skip 2 levels
        //     0b11 = Skip 3 levels
        pub(crate) skl@[2:1] as SkipLevel {
            Skip0Level = 0b00,
            Skip1Level = 0b01,
            Skip2Level = 0b10,
            Skip3Level = 0b11,
        },

        reserved@[4:3] [res0],

        // BADDR — Translation table base address:
        //   Bits A[47:x] of the stage-2 base address are held here.
        //   Bits A[(x-1):0] are zero (alignment to the size of the base table),
        //   where x depends on VTCR_EL2.{TG0,SL0,SL2,DS} and the effective start level.
        //   Note: With larger OA sizes (e.g., 52-bit when permitted), higher address bits
        //   are only accessible when the 128-bit form of VTTBR_EL2 is enabled.
        pub(crate) baddr@[47:5],

        // VMID — Virtual Machine Identifier:
        //   When FEAT_VMID16 is implemented and VTCR_EL2.VS==1: full [63:48] used (16-bit VMID).
        //   Otherwise: upper eight bits [63:56] are RES0, yielding an 8-bit VMID.
        pub(crate) vmid@[63:48]
    }
}

bitregs! {
    /// HCR_EL2 — Hypervisor Configuration Register
    /// Purpose:
    ///     Provides virtualization configuration controls, including whether various
    ///     Non-secure EL1/EL0 operations are trapped to EL2 and how exceptions are routed.
    pub(crate) struct HCR_EL2: u64 {
        // [0..15]
        // Enable stage 2 translation for Non-secure EL1&0.
        //   0b0: Stage 2 translation disabled
        //   0b1: Stage 2 translation enabled
        pub(crate) vm@[0:0],

        // Set/Way Invalidation Override for cache maintenance by set/way.
        pub(crate) swio@[1:1],

        // Permission fault on S1 page-table walks that access Device memory.
        //   0b1: If a stage-1 walk touches Device memory, take a stage-2 Permission fault
        pub(crate) ptw@[2:2],

        // Route physical FIQ/IRQ/SError taken at EL1/EL0 to EL2.
        pub(crate) fmo@[3:3],   // FIQ routing to EL2
        pub(crate) imo@[4:4],   // IRQ routing to EL2
        pub(crate) amo@[5:5],   // SError routing to EL2

        // Inject virtual exceptions for the guest:
        //   VF: vFIQ pending, VI: vIRQ pending, VSE: vSError pending
        pub(crate) vf@[6:6],
        pub(crate) vi@[7:7],
        pub(crate) vse@[8:8],

        // Force broadcast of certain maintenance ops to the required shareability domain.
        pub(crate) fb@[9:9],

        // Barrier Shareability Upgrade for DSB/ISB executed at EL1/EL0.
        pub(crate) bsu@[11:10],

        // Default Cacheability when S1 MMU is disabled at EL1/EL0.
        //   0b1: Treat accesses as Normal WB cacheable
        pub(crate) dc@[12:12],

        // Trap WFI/WFE executed at EL1/EL0 to EL2.
        pub(crate) twi@[13:13], // WFI trap
        pub(crate) twe@[14:14], // WFE trap

        // Trap reads of ID group 0/1/2/3 registers at EL1/EL0 to EL2.
        pub(crate) tid0@[15:15],

        // [16..31]
        pub(crate) tid1@[16:16],
        pub(crate) tid2@[17:17],
        pub(crate) tid3@[18:18],

        // Trap SMC executed at Non-secure EL1/EL0 to EL2.
        pub(crate) tsc@[19:19],

        // Trap IMPLEMENTATION DEFINED system-register accesses at EL1/EL0 to EL2.
        pub(crate) tidcp@[20:20],

        // Trap Auxiliary Control Register accesses at EL1/EL0 to EL2.
        pub(crate) tacr@[21:21],

        // Trap cache maintenance by set/way at EL1/EL0 to EL2.
        pub(crate) tsw@[22:22],

        // Trap cache maintenance to Point of Coherency / Physical Storage at EL1 to EL2.
        pub(crate) tpcp@[23:23],

        // Trap cache maintenance to Point of Unification at EL1 to EL2.
        pub(crate) tpu@[24:24],

        // Trap TLB maintenance instructions at EL1 to EL2.
        pub(crate) ttlb@[25:25],

        // Trap virtual memory control (TTBRx_EL1/TCR_EL1/SCTLR_EL1 writes等) at EL1 to EL2.
        pub(crate) tvm@[26:26],

        // Route general exceptions from EL0 to EL2 when E2H==1 (VHE mode interaction).
        pub(crate) tge@[27:27],

        // Trap DC ZVA at EL1/EL0 to EL2.
        pub(crate) tdz@[28:28],

        // HVC instruction disable (UNDEFINED at EL1/EL2 when set; does not trap).
        pub(crate) hcd@[29:29],

        // Trap reads of certain virtual memory controls at EL1 to EL2.
        pub(crate) trvm@[30:30],

        // Execution state for the next-lower EL (0 = AArch32, 1 = AArch64).
        pub(crate) rw@[31:31],

        // [32..47]
        // Stage-2 cacheability disable:
        //   CD: force S2 data accesses/table walks to Non-cacheable
        //   ID: force S2 instruction fetches to Non-cacheable
        pub(crate) cd@[32:32],
        pub(crate) id@[33:33],

        // E2H — EL2 as host (VHE). Requires FEAT_VHE.
        //   When FEAT_E2H0 is not implemented, this field can be RES1 (behaves as 1 except on direct read).
        //   Otherwise if FEAT_VHE is not implemented: RES0
        pub(crate) e2h@[34:34],

        // TLOR — Trap LORegion registers to EL2. Requires FEAT_LOR, otherwise RES0.
        pub(crate) tlor@[35:35],

        // TERR — Trap RAS Error Record registers to EL2. Requires FEAT_RAS, otherwise RES0.
        pub(crate) terr@[36:36],

        // TEA — Route synchronous External aborts to EL2. Requires FEAT_RAS, otherwise RES0.
        pub(crate) tea@[37:37],

        // Reserved (was MIOCNCE). RES0.
        reserved@[38:38] [res0],

        // TME — Transactional Memory enable for lower ELs. Requires FEAT_TME, otherwise RES0.
        pub(crate) tme@[39:39],

        // APK/API — Pointer Authentication traps. Require FEAT_PAuth, otherwise RES0.
        pub(crate) apk@[40:40],
        pub(crate) api@[41:41],

        // Nested virtualization controls:
        //   NV  — base nested-virt trap/redirection control (FEAT_NV or FEAT_NV2)
        //   NV1 — additional NV behaviors (FEAT_NV or FEAT_NV2)
        //   AT  — trap AT S1E1* / S1E0* (FEAT_NV; S1E1A additionally requires FEAT_ATS1A)
        //   NV2 — enhanced nested-virt (FEAT_NV2)
        // Not implemented features: corresponding fields are RES0.
        pub(crate) nv@[42:42],
        pub(crate) nv1@[43:43],
        pub(crate) at@[44:44],
        pub(crate) nv2@[45:45],

        // FWB — Stage-2 Forced Write-Back combining. Requires FEAT_S2FWB, otherwise RES0.
        pub(crate) fwb@[46:46],

        // FIEN — RAS Fault Injection enable. Requires FEAT_RASv1p1, otherwise RES0.
        pub(crate) fien@[47:47],

        // [48..63]
        // GPF — Route Granule Protection Faults to EL2. Requires FEAT_RME, otherwise RES0.
        pub(crate) gpf@[48:48],

        // TID4 — Trap ID group 4 to EL2. Requires FEAT_EVT (Enhanced Virtualization Traps), otherwise RES0.
        pub(crate) tid4@[49:49],

        // TICAB — Trap IC IALLUIS/ICIALLUIS to EL2. Requires FEAT_EVT, otherwise RES0.
        pub(crate) ticab@[50:50],

        // AMVOFFEN — AMU virtualization via virtual offsets. Requires FEAT_AMUv1p1, otherwise RES0.
        pub(crate) amvoffen@[51:51],

        // TOCU — Trap cache maintenance to PoU (IC IVAU/IC IALLU/DC CVAU etc.). Requires FEAT_EVT, otherwise RES0.
        pub(crate) tocu@[52:52],

        // EnSCXT — Access to SCXTNUM_EL1/EL0 (no trap when set).
        // Requires FEAT_CSV2_2 or FEAT_CSV2_1p2, otherwise RES0.
        pub(crate) enscxt@[53:53],

        // Fine-grained TLB maintenance traps:
        //   TTLBIS — trap *IS (Inner Shareable) TLBI; requires FEAT_EVT, otherwise RES0.
        //   TTLBOS — trap *OS (Outer Shareable) TLBI; requires FEAT_EVT, otherwise RES0.
        pub(crate) ttlbis@[54:54],
        pub(crate) ttlbos@[55:55],

        // MTE controls for lower ELs:
        //   ATA — Allocation Tag Access control; requires FEAT_MTE2, otherwise RES0.
        //   DCT — Default Cacheability Tagging with HCR_EL2.DC; requires FEAT_MTE2, otherwise RES0.
        pub(crate) ata@[56:56],
        pub(crate) dct@[57:57],

        // TID5 — Trap ID group 5 (e.g., GMID_EL1). Requires FEAT_MTE2, otherwise RES0.
        pub(crate) tid5@[58:58],

        // TWED — Trap WFE Exception Delay:
        //   TWEDEn — enable; TWEDEL — delay encoding 2^(TWEDEL+8) cycles
        //   Both require FEAT_TWED; otherwise RES0.
        pub(crate) tweden@[59:59],
        pub(crate) twedel@[63:60]
    }
}
