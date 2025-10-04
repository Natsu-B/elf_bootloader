use typestate::bitregs;

bitregs! {
    /// VMSAv8-64 Stage 2 Table descriptor format (48-bit output address)
    /// Applies to 4KB, 16KB, and 64KB granules.
    /// Assumptions:
    ///   - FEAT_LPA2 is NOT in use (i.e., VTCR_EL2.DS == 0): bits[49:48] are RES0 and
    ///     52-bit output addresses are not described by descriptors. See VTCR_EL2.DS.
    ///   - Stage 2 Table descriptors carry no NSTable/APTable/PXNTable/UXNTable fields.
    pub(crate) struct Stage2_48bitTableDescriptor: u64 {
        // Descriptor type for a Table: must be 0b11.
        reserved@[1:0] [res1],

        // Lower attribute bits in a Table descriptor at Stage 2 are ignored by hardware.
        // (e.g., SH[1:0] at [9:8] are ignored for Table descriptors)
        reserved@[11:2] [ignore],

        // Next-level table address (alignment depends on granule)
        //   - 4KB granule:  [47:12] used
        //   - 16KB granule: [47:14] used
        //   - 64KB granule: [47:16] used
        NLTA@[47:12],

        // For 48-bit OA (no FEAT_LPA2 / DS==0), these bits are RES0 in translation descriptors.
        reserved@[50:48] [res0],

        // Upper bits in Stage 2 Table descriptors are ignored by hardware.
        reserved@[58:51] [ignore],

        // Stage 2 Table descriptors define no NSTable/APTable/UXNTable/PXNTable:
        // always RES0/ignored irrespective of permission indirection (S2PIE).
        reserved@[62:59] [res0],

        // Bit[63] (NSTable) exists only at Stage 1; at Stage 2 this is RES0.
        reserved@[63:63] [res0],
    }
}

bitregs! {
    /// VMSAv8-64 Stage 2 Block descriptor format (48-bit OA)
    /// Valid for 4KB, 16KB, and 64KB granules.
    /// Leaf entries appear at level 1 or 2 (no blocks at level 3).
    pub(crate) struct Stage2_48bitBlockDescriptor: u64 {
        // Descriptor type = Block (bits[1:0] == 0b01)
        // [0] must be 1 for a valid descriptor.
        reserved@[0:0] [res1],
        // [1] must be 0 for a Block descriptor.
        reserved@[1:1] [res0],

        // MemAttr[3:0] — Stage-2 memory type & cacheability (Device/Normal, inner/outer).
        // NOTE (FEAT_S2FWB): When implemented and enabled (HCR_EL2.FWB==1),
        //   the combination rules for S1/S2 cacheability follow S2FWB semantics.
        pub(crate) mem_attr@[5:2],

        // S2AP[1:0] — Stage-2 access permissions (combined with Stage-1 permissions).
        // NOTE (FEAT_HAFDBS family): With hardware-managed Access/Dirty state,
        //   DBM can interact with S2AP for write-dirty tracking.
        pub(crate) s2ap0@[6:6],
        pub(crate) s2ap1@[7:7],

        // SH — Shareability for Normal memory:
        //   0b00=Non-shareable, 0b10=Outer Shareable, 0b11=Inner Shareable.
        // NOTE (FEAT_LPA2; VTCR_EL2.DS==1):
        //   bits[9:8] are repurposed as OA[51:50] (upper output address bits);
        //   in that case shareability is selected by VTCR_EL2.SH0 (not in the descriptor).
        pub(crate) sh@[9:8],

        // AF — Access Flag:
        //   0: first access takes AF fault (unless hardware AF update is enabled),
        //   1: access permitted (subject to permissions).
        pub(crate) af@[10:10],

        // [11] — not used at Stage 2 (nG is Stage-1 only). Must be RES0.
        reserved@[11:11] [res0],

        // OA base — Output Address (Block address).
        // Block lower bits are zeroed according to level & TG:
        //   TG=4KB : L0->512GiB (OA[47:39] valid) L1->1GiB (OA[47:30] valid), L2->2MiB (OA[47:21] valid)
        //   TG=16KB: L1->512MiB (OA[47:34]),  L2->32MiB (OA[47:25])
        //   TG=64KB: L1->256MiB (OA[47:36]),  L2->512KiB (OA[47:29])
        // We model the superset slice and expect SW to keep the extra low bits zero.
        // NOTE (FEAT_LPA2; VTCR_EL2.DS==1):
        //   OA[49:48] live in descriptor bits[49:48], and OA[51:50] live in bits[9:8].
        pub(crate) oab@[47:21],

        // nT — “No-translate” hint for size-change sequences.
        //   Requires FEAT_BBML1. When set, implementation may avoid caching this
        //   translation and can fault instead of caching to avoid TLB conflicts.
        //   Otherwise: RES0.
        pub(crate) nt@[16:16],

        // Keep these RES0 in the 48-bit OA format (they carry OA bits when DS==1).
        // NOTE (FEAT_LPA2; VTCR_EL2.DS==1): bits[49:48] hold OA[49:48].
        reserved@[50:48] [res0],

        // DBM — Dirty Bit Modifier (hardware dirty logging support).
        //   Requires FEAT_HAFDBS (and related dirty-state features). Otherwise: RES0.
        pub(crate) dbm@[51:51],

        // Contiguous — 16 adjacent entries hint a larger mapping.
        //   Performance hint; implementations may ignore.
        //   NOTE (FEAT_BBML1/BBML2): update/relaxation rules for TLB conflicts may differ.
        pub(crate) contiguous@[52:52],

        // Execute-never control:
        //   Without FEAT_XNX: bit[54] is a single XN for all ELs; bit[53] is RES0.
        //   With    FEAT_XNX: [54]=UXN (EL0 XN), [53]=PXN (EL1+ XN).
        pub(crate) xn@[54:53],

        // NS — Security attribute of the *output* address.
        //   Secure state translations only: 0=Secure, 1=Non-secure.
        //   Non-secure translations: architecturally ignored/treated as Non-secure.
        pub(crate) ns@[55:55],

        // Software-reserved (ignored by hardware).
        pub(crate) sw@[57:56],

        // AssuredOnly (bit[58]) — only when FEAT_THE is implemented AND enabled by VTCR_EL2.
        //   If FEAT_THE is not implemented or not enabled for Stage 2: RES0.
        //   NOTE: If the Stage-2 translation system is 128-bit (VTCR_EL2.D128==1),
        //   this field is defined RES0 by the architecture.
        pub(crate) assured_only@[58:58],

        // Implementation-defined / ignored by CPU.
        //   Some SMMUs may internally use these, but the PE treats them as ignored.
        reserved@[59:59] [ignore],
        reserved@[62:60] [ignore],

        // Top bit must be RES0 (kept for forward compatibility).
        reserved@[63:63] [res0],
    }
}
