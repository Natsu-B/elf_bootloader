use crate::GicError;
use crate::IrqGroup;
use crate::TriggerMode;
use crate::VIntId;
use crate::VcpuId;
use crate::VgicIrqScope;

pub(crate) const SGI_COUNT: usize = 16;
pub(crate) const LOCAL_INTID_COUNT: usize = 32;

#[derive(Copy, Clone)]
pub(crate) struct IrqAttrs {
    pub(crate) pending: bool,
    pub(crate) active: bool,
    pub(crate) enable: bool,
    pub(crate) group: IrqGroup,
    pub(crate) priority: u8,
}

#[derive(Copy, Clone)]
enum BoolField {
    Group,
    Enable,
    Pending,
    Active,
}

pub(crate) struct IrqState<const VCPUS: usize>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    vcpu_count: usize,
    group_local: [u32; VCPUS],
    enable_local: [u32; VCPUS],
    pending_local: [u32; VCPUS],
    active_local: [u32; VCPUS],
    priority_local: [[u8; LOCAL_INTID_COUNT]; VCPUS],
    trigger_local: [[TriggerMode; LOCAL_INTID_COUNT]; VCPUS],

    group_global: [u32; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
    enable_global: [u32; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
    pending_global: [u32; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
    active_global: [u32; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
    priority_global: [u8; crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT],
    trigger_global: [TriggerMode; crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT],
}

impl<const VCPUS: usize> IrqState<VCPUS>
where
    [(); crate::max_intids_for_vcpus(VCPUS)]:,
    [(); crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT]:,
    [(); (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32]:,
{
    pub(crate) fn new(vcpu_count: usize) -> Self {
        let mut trigger_local = [[TriggerMode::Level; LOCAL_INTID_COUNT]; VCPUS];
        for t in trigger_local.iter_mut().take(vcpu_count) {
            for intid in 0..SGI_COUNT {
                t[intid] = TriggerMode::Edge;
            }
        }
        Self {
            vcpu_count,
            group_local: [0; VCPUS],
            enable_local: [0; VCPUS],
            pending_local: [0; VCPUS],
            active_local: [0; VCPUS],
            priority_local: [[0u8; LOCAL_INTID_COUNT]; VCPUS],
            trigger_local,

            group_global: [0; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
            enable_global: [0; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
            pending_global: [0; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
            active_global: [0; (crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT + 31) / 32],
            priority_global: [0u8; { crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT }],
            trigger_global: [TriggerMode::Level; {
                crate::max_intids_for_vcpus(VCPUS) - LOCAL_INTID_COUNT
            }],
        }
    }

    pub(crate) fn vcpu_index(&self, id: VcpuId) -> Result<usize, GicError> {
        if (id.0 as usize) < self.vcpu_count {
            Ok(id.0 as usize)
        } else {
            Err(GicError::InvalidVcpuId)
        }
    }

    pub(crate) fn intid_in_range(intid: usize) -> bool {
        intid < crate::max_intids_for_vcpus(VCPUS)
    }

    #[inline(always)]
    fn lsb_mask(bits: usize) -> u32 {
        if bits >= u32::BITS as usize {
            u32::MAX
        } else {
            (1u32 << bits) - 1
        }
    }

    #[inline(always)]
    fn valid_window_bits(base: usize) -> usize {
        crate::max_intids_for_vcpus(VCPUS)
            .saturating_sub(base)
            .min(32)
    }

    #[inline(always)]
    fn local_bit_mask(intid: usize) -> u32 {
        1u32 << intid
    }

    #[inline(always)]
    fn global_bit_index(intid: usize) -> usize {
        intid - LOCAL_INTID_COUNT
    }

    #[inline(always)]
    fn global_word_and_mask(intid: usize) -> (usize, u32) {
        let bit_index = Self::global_bit_index(intid);
        (bit_index / 32, 1u32 << (bit_index % 32))
    }

    fn local_window(base: usize) -> Result<Option<(usize, u32, u32)>, GicError> {
        if !Self::intid_in_range(base) {
            return Ok(None);
        }
        if base >= LOCAL_INTID_COUNT {
            return Err(GicError::UnsupportedIntId);
        }
        let valid_bits = Self::valid_window_bits(base);
        if base + valid_bits > LOCAL_INTID_COUNT {
            return Err(GicError::UnsupportedIntId);
        }
        let valid_mask = Self::lsb_mask(valid_bits);
        Ok(Some((base, valid_mask, valid_mask << base)))
    }

    fn global_window(base: usize) -> Result<Option<(usize, usize, u32)>, GicError> {
        if !Self::intid_in_range(base) {
            return Ok(None);
        }
        if base < LOCAL_INTID_COUNT {
            return Err(GicError::UnsupportedIntId);
        }
        let bit_index = Self::global_bit_index(base);
        let valid_bits = Self::valid_window_bits(base);
        let valid_mask = Self::lsb_mask(valid_bits);
        Ok(Some((bit_index / 32, bit_index % 32, valid_mask)))
    }

    #[inline(always)]
    fn read_u64_pair(words: &[u32], word_index: usize) -> u64 {
        let w0 = words[word_index] as u64;
        let w1 = words.get(word_index + 1).copied().unwrap_or(0) as u64;
        w0 | (w1 << 32)
    }

    #[inline(always)]
    fn write_u64_pair(words: &mut [u32], word_index: usize, pair: u64) {
        words[word_index] = pair as u32;
        if let Some(next) = words.get_mut(word_index + 1) {
            *next = (pair >> 32) as u32;
        }
    }

    #[inline(always)]
    fn read_window_from_pair(pair: u64, shift: usize) -> u32 {
        ((pair >> shift) & (u32::MAX as u64)) as u32
    }

    #[inline(always)]
    fn write_window_to_pair(pair: u64, shift: usize, valid_mask: u32, window: u32) -> u64 {
        let bit_mask = (valid_mask as u64) << shift;
        (pair & !bit_mask) | (((window & valid_mask) as u64) << shift)
    }

    fn select_bool_slices(&self, field: BoolField) -> (&[u32], &[u32]) {
        match field {
            BoolField::Group => (&self.group_local[..self.vcpu_count], &self.group_global),
            BoolField::Enable => (&self.enable_local[..self.vcpu_count], &self.enable_global),
            BoolField::Pending => (&self.pending_local[..self.vcpu_count], &self.pending_global),
            BoolField::Active => (&self.active_local[..self.vcpu_count], &self.active_global),
        }
    }

    fn select_bool_local_mut(&mut self, field: BoolField) -> &mut [u32] {
        match field {
            BoolField::Group => &mut self.group_local[..self.vcpu_count],
            BoolField::Enable => &mut self.enable_local[..self.vcpu_count],
            BoolField::Pending => &mut self.pending_local[..self.vcpu_count],
            BoolField::Active => &mut self.active_local[..self.vcpu_count],
        }
    }

    fn select_bool_global_mut(&mut self, field: BoolField) -> &mut [u32] {
        match field {
            BoolField::Group => &mut self.group_global[..],
            BoolField::Enable => &mut self.enable_global[..],
            BoolField::Pending => &mut self.pending_global[..],
            BoolField::Active => &mut self.active_global[..],
        }
    }

    fn read_bool_word(
        &self,
        scope: VgicIrqScope,
        base: usize,
        field: BoolField,
    ) -> Result<u32, GicError> {
        let (local, global) = self.select_bool_slices(field);
        match scope {
            VgicIrqScope::Local(vcpu) => {
                let idx = self.vcpu_index(vcpu)?;
                let Some((shift, valid_mask, _)) = Self::local_window(base)? else {
                    return Ok(0);
                };
                Ok((local[idx] >> shift) & valid_mask)
            }
            VgicIrqScope::Global => {
                let Some((word_index, shift, valid_mask)) = Self::global_window(base)? else {
                    return Ok(0);
                };
                let pair = Self::read_u64_pair(global, word_index);
                let window = Self::read_window_from_pair(pair, shift);
                Ok(window & valid_mask)
            }
        }
    }

    // Apply one register-window transform and report exactly which stored bits changed.
    fn update_bool_word(
        &mut self,
        scope: VgicIrqScope,
        base: usize,
        field: BoolField,
        update: impl FnOnce(u32) -> u32,
    ) -> Result<u32, GicError> {
        match scope {
            VgicIrqScope::Local(vcpu) => {
                let idx = self.vcpu_index(vcpu)?;
                let Some((shift, valid_mask, word_mask)) = Self::local_window(base)? else {
                    return Ok(0);
                };
                let local = self.select_bool_local_mut(field);
                let slot = &mut local[idx];
                let current = (*slot >> shift) & valid_mask;
                let new = update(current) & valid_mask;
                *slot = (*slot & !word_mask) | (new << shift);
                Ok(current ^ new)
            }
            VgicIrqScope::Global => {
                let Some((word_index, shift, valid_mask)) = Self::global_window(base)? else {
                    return Ok(0);
                };
                let global = self.select_bool_global_mut(field);
                let pair = Self::read_u64_pair(global, word_index);
                let current = Self::read_window_from_pair(pair, shift) & valid_mask;
                let new = update(current) & valid_mask;
                let new_pair = Self::write_window_to_pair(pair, shift, valid_mask, new);
                Self::write_u64_pair(global, word_index, new_pair);
                Ok(current ^ new)
            }
        }
    }

    fn set_bool(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        val: bool,
        field: BoolField,
    ) -> Result<bool, GicError> {
        let intid = vintid.0 as usize;
        if !Self::intid_in_range(intid) {
            return Err(GicError::UnsupportedIntId);
        }
        match scope {
            VgicIrqScope::Local(vcpu) => {
                if intid >= LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = self.vcpu_index(vcpu)?;
                let local = self.select_bool_local_mut(field);
                let bit = Self::local_bit_mask(intid);
                let slot = &mut local[idx];
                let old = (*slot & bit) != 0;
                let changed = old != val;
                if val {
                    *slot |= bit;
                } else {
                    *slot &= !bit;
                }
                Ok(changed)
            }
            VgicIrqScope::Global => {
                if intid < LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let global = self.select_bool_global_mut(field);
                let (word_index, bit) = Self::global_word_and_mask(intid);
                let slot = &mut global[word_index];
                let old = (*slot & bit) != 0;
                let changed = old != val;
                if val {
                    *slot |= bit;
                } else {
                    *slot &= !bit;
                }
                Ok(changed)
            }
        }
    }

    pub(crate) fn irq_attrs(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
    ) -> Result<IrqAttrs, GicError> {
        let intid = vintid.0 as usize;
        if !Self::intid_in_range(intid) {
            return Err(GicError::UnsupportedIntId);
        }
        match scope {
            VgicIrqScope::Local(vcpu) => {
                if intid >= LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = self.vcpu_index(vcpu)?;
                let group = (self.group_local[idx] & Self::local_bit_mask(intid)) != 0;
                let enable = (self.enable_local[idx] & Self::local_bit_mask(intid)) != 0;
                let pending = (self.pending_local[idx] & Self::local_bit_mask(intid)) != 0;
                let active = (self.active_local[idx] & Self::local_bit_mask(intid)) != 0;
                Ok(IrqAttrs {
                    pending,
                    active,
                    enable,
                    group: if group {
                        IrqGroup::Group1
                    } else {
                        IrqGroup::Group0
                    },
                    priority: self.priority_local[idx][intid],
                })
            }
            VgicIrqScope::Global => {
                if intid < LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let (word_index, bit) = Self::global_word_and_mask(intid);
                let group = (self.group_global[word_index] & bit) != 0;
                let enable = (self.enable_global[word_index] & bit) != 0;
                let pending = (self.pending_global[word_index] & bit) != 0;
                let active = (self.active_global[word_index] & bit) != 0;
                let idx = intid - LOCAL_INTID_COUNT;
                Ok(IrqAttrs {
                    pending,
                    active,
                    enable,
                    group: if group {
                        IrqGroup::Group1
                    } else {
                        IrqGroup::Group0
                    },
                    priority: self.priority_global[idx],
                })
            }
        }
    }

    pub(crate) fn trigger_mode(
        &self,
        scope: VgicIrqScope,
        vintid: VIntId,
    ) -> Result<TriggerMode, GicError> {
        let intid = vintid.0 as usize;
        if !Self::intid_in_range(intid) {
            return Err(GicError::UnsupportedIntId);
        }
        match scope {
            VgicIrqScope::Local(vcpu) => {
                if intid >= LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = self.vcpu_index(vcpu)?;
                Ok(self.trigger_local[idx][intid])
            }
            VgicIrqScope::Global => {
                if intid < LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                Ok(self.trigger_global[intid - LOCAL_INTID_COUNT])
            }
        }
    }

    pub(crate) fn set_group(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        group: IrqGroup,
    ) -> Result<bool, GicError> {
        let val = matches!(group, IrqGroup::Group1);
        self.set_bool(scope, vintid, val, BoolField::Group)
    }

    pub(crate) fn set_enable(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        enable: bool,
    ) -> Result<bool, GicError> {
        self.set_bool(scope, vintid, enable, BoolField::Enable)
    }

    pub(crate) fn set_pending(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        pending: bool,
    ) -> Result<bool, GicError> {
        self.set_bool(scope, vintid, pending, BoolField::Pending)
    }

    pub(crate) fn set_priority(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        priority: u8,
    ) -> Result<bool, GicError> {
        let intid = vintid.0 as usize;
        if !Self::intid_in_range(intid) {
            return Err(GicError::UnsupportedIntId);
        }
        Ok(match scope {
            VgicIrqScope::Local(vcpu) => {
                if intid >= LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = self.vcpu_index(vcpu)?;
                let slot = &mut self.priority_local[idx][intid];
                core::mem::replace(slot, priority) != priority
            }
            VgicIrqScope::Global => {
                if intid < LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = intid - LOCAL_INTID_COUNT;
                core::mem::replace(&mut self.priority_global[idx], priority) != priority
            }
        })
    }

    pub(crate) fn set_trigger(
        &mut self,
        scope: VgicIrqScope,
        vintid: VIntId,
        trigger: TriggerMode,
    ) -> Result<bool, GicError> {
        let intid = vintid.0 as usize;
        if !Self::intid_in_range(intid) {
            return Err(GicError::UnsupportedIntId);
        }
        Ok(match scope {
            VgicIrqScope::Local(vcpu) => {
                if intid >= LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = self.vcpu_index(vcpu)?;
                let slot = &mut self.trigger_local[idx][intid];
                core::mem::replace(slot, trigger) != trigger
            }
            VgicIrqScope::Global => {
                if intid < LOCAL_INTID_COUNT {
                    return Err(GicError::UnsupportedIntId);
                }
                let idx = intid - LOCAL_INTID_COUNT;
                core::mem::replace(&mut self.trigger_global[idx], trigger) != trigger
            }
        })
    }

    pub(crate) fn read_group_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
    ) -> Result<u32, GicError> {
        self.read_bool_word(scope, base.0 as usize, BoolField::Group)
    }

    pub(crate) fn write_group_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        value: u32,
    ) -> Result<bool, GicError> {
        Ok(self.update_bool_word(scope, base.0 as usize, BoolField::Group, |_| value)? != 0)
    }

    pub(crate) fn read_enable_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
    ) -> Result<u32, GicError> {
        self.read_bool_word(scope, base.0 as usize, BoolField::Enable)
    }

    pub(crate) fn write_set_enable_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Enable, |current| {
            current | set_bits
        })
    }

    pub(crate) fn write_clear_enable_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Enable, |current| {
            current & !clear_bits
        })
    }

    pub(crate) fn read_pending_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
    ) -> Result<u32, GicError> {
        self.read_bool_word(scope, base.0 as usize, BoolField::Pending)
    }

    pub(crate) fn write_set_pending_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Pending, |current| {
            current | set_bits
        })
    }

    pub(crate) fn write_clear_pending_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Pending, |current| {
            current & !clear_bits
        })
    }

    pub(crate) fn read_active_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
    ) -> Result<u32, GicError> {
        self.read_bool_word(scope, base.0 as usize, BoolField::Active)
    }

    pub(crate) fn write_set_active_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        set_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Active, |current| {
            current | set_bits
        })
    }

    pub(crate) fn write_clear_active_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        clear_bits: u32,
    ) -> Result<u32, GicError> {
        self.update_bool_word(scope, base.0 as usize, BoolField::Active, |current| {
            current & !clear_bits
        })
    }

    pub(crate) fn read_priority_word_raw(
        &self,
        scope: VgicIrqScope,
        base: usize,
    ) -> Result<u32, GicError> {
        let mut bytes = [0u8; 4];
        for lane in 0..4 {
            let intid = base + lane;
            if !Self::intid_in_range(intid) {
                break;
            }
            bytes[lane] = match scope {
                VgicIrqScope::Local(vcpu) => {
                    if intid >= LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let idx = self.vcpu_index(vcpu)?;
                    self.priority_local[idx][intid]
                }
                VgicIrqScope::Global => {
                    if intid < LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    self.priority_global[intid - LOCAL_INTID_COUNT]
                }
            };
        }
        Ok(u32::from_le_bytes(bytes))
    }

    pub(crate) fn write_priority_word_raw(
        &mut self,
        scope: VgicIrqScope,
        base: usize,
        value: u32,
    ) -> Result<bool, GicError> {
        let mut changed = false;
        for lane in 0..4 {
            let intid = base + lane;
            if !Self::intid_in_range(intid) {
                break;
            }
            let prio = ((value >> (lane * 8)) & 0xff) as u8;
            match scope {
                VgicIrqScope::Local(vcpu) => {
                    if intid >= LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let idx = self.vcpu_index(vcpu)?;
                    let slot = &mut self.priority_local[idx][intid];
                    if *slot != prio {
                        *slot = prio;
                        changed = true;
                    }
                }
                VgicIrqScope::Global => {
                    if intid < LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let slot = &mut self.priority_global[intid - LOCAL_INTID_COUNT];
                    if *slot != prio {
                        *slot = prio;
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    pub(crate) fn read_trigger_word(
        &self,
        scope: VgicIrqScope,
        base: VIntId,
    ) -> Result<u32, GicError> {
        let mut value = 0u32;
        for bit in 0..16 {
            let intid = base.0 as usize + bit;
            if !Self::intid_in_range(intid) {
                break;
            }
            let is_edge = match scope {
                VgicIrqScope::Local(vcpu) => {
                    if intid >= LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let idx = self.vcpu_index(vcpu)?;
                    self.trigger_local[idx][intid] == TriggerMode::Edge
                }
                VgicIrqScope::Global => {
                    if intid < LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    self.trigger_global[intid - LOCAL_INTID_COUNT] == TriggerMode::Edge
                }
            };
            if is_edge {
                value |= 0b10 << (bit * 2);
            }
        }
        Ok(value)
    }

    pub(crate) fn write_trigger_word(
        &mut self,
        scope: VgicIrqScope,
        base: VIntId,
        value: u32,
    ) -> Result<bool, GicError> {
        let mut changed = false;
        for bit in 0..16 {
            let intid = base.0 as usize + bit;
            if !Self::intid_in_range(intid) {
                break;
            }
            let field = (value >> (bit * 2)) & 0b11;
            let trigger = if field == 0b10 {
                TriggerMode::Edge
            } else {
                TriggerMode::Level
            };
            match scope {
                VgicIrqScope::Local(vcpu) => {
                    if intid >= LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let idx = self.vcpu_index(vcpu)?;
                    let slot = &mut self.trigger_local[idx][intid];
                    if *slot != trigger {
                        *slot = trigger;
                        changed = true;
                    }
                }
                VgicIrqScope::Global => {
                    if intid < LOCAL_INTID_COUNT {
                        return Err(GicError::UnsupportedIntId);
                    }
                    let slot = &mut self.trigger_global[intid - LOCAL_INTID_COUNT];
                    if *slot != trigger {
                        *slot = trigger;
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }
}
