//! Overlay application for Device Tree blobs.
//!
//! This implements the libfdt-style merge plus a project-specific deletion
//! extension.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::ValueRef;
use crate::ast::DeviceTree;
use crate::ast::DeviceTreeEditExt;
use crate::ast::DeviceTreeQueryExt;
use crate::ast::NameRef;
use crate::ast::Node;
use crate::ast::NodeEditExt;
use crate::ast::NodeId;
use crate::ast::NodeQueryExt;
use crate::ast::Owned;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum OverlayError {
    MissingFragmentOverlay,
    MissingFragmentTarget,
    MalformedOverlay,
    InvalidTargetPath,
    InvalidTargetPhandle,
    TargetNotFound,
    MissingSymbols,
    SymbolNotFound,
    MalformedFixup,
    FixupPropertyMissing,
    FixupOffsetOutOfBounds,
    LocalFixupMissingNode,
    LocalFixupMissingProperty,
    LocalFixupOffsetOutOfBounds,
    InvalidPhandle,
    PhandleOverflow,
    DuplicatePhandle,
    DeletesNotAllowed,
    StrictViolation,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OverlayApplyOptions {
    pub update_symbols: bool,
    pub allow_custom_deletes: bool,
    pub strict: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OverlayApplyReport {
    pub fragments_applied: usize,
    pub properties_set: usize,
    pub nodes_added: usize,
    pub properties_deleted: usize,
    pub nodes_deleted: usize,
    pub symbols_updated: usize,
    pub modified_nodes: Vec<String>,
}

#[derive(Debug, Clone)]
enum FragmentTarget {
    Phandle(u32),
    Path(String),
}

#[derive(Debug, Clone)]
struct FragmentInfo {
    name: String,
    target: FragmentTarget,
    overlay_root: NodeId,
}

impl OverlayApplyReport {
    fn record_modified_node(&mut self, path: String) {
        if self.modified_nodes.last().map(|p| p == &path) == Some(true) {
            return;
        }
        self.modified_nodes.push(path);
    }

    fn merge_from(&mut self, other: OverlayApplyReport) {
        self.fragments_applied += other.fragments_applied;
        self.properties_set += other.properties_set;
        self.nodes_added += other.nodes_added;
        self.properties_deleted += other.properties_deleted;
        self.nodes_deleted += other.nodes_deleted;
        self.symbols_updated += other.symbols_updated;
        self.modified_nodes
            .extend(other.modified_nodes.into_iter().filter(|p| !p.is_empty()));
    }
}

impl<'dtb> DeviceTree<'dtb, Owned> {
    pub fn apply_overlay<'ov>(
        &mut self,
        overlay_tree: &DeviceTree<'ov>,
        opt: OverlayApplyOptions,
    ) -> Result<OverlayApplyReport, OverlayError> {
        let mut working_overlay = overlay_tree.clone();
        let (phandle_index, max_phandle) = build_phandle_index(self)?;
        let delta = max_phandle;

        adjust_overlay_phandles(&mut working_overlay, delta)?;
        apply_local_fixups(&mut working_overlay, delta)?;
        apply_external_fixups(self, &mut working_overlay)?;
        let fragments = collect_fragments(&working_overlay)?;

        let mut report = OverlayApplyReport::default();
        for fragment in &fragments {
            let target_node = resolve_fragment_target(self, &phandle_index, fragment)?;
            merge_overlay_subtree(
                self,
                &working_overlay,
                fragment.overlay_root,
                target_node,
                &opt,
                &mut report,
            )?;
            report.fragments_applied += 1;
        }

        if opt.update_symbols {
            update_symbols(self, &working_overlay, &fragments, &mut report)?;
        }

        Ok(report)
    }

    pub fn apply_overlays<'ov, I>(
        &mut self,
        overlays: I,
        opt: OverlayApplyOptions,
    ) -> Result<OverlayApplyReport, OverlayError>
    where
        I: IntoIterator<Item = &'ov DeviceTree<'ov>>,
    {
        let mut report = OverlayApplyReport::default();
        for overlay in overlays {
            let r = self.apply_overlay(overlay, opt)?;
            report.merge_from(r);
        }
        Ok(report)
    }
}

fn collect_fragments(overlay: &DeviceTree<'_>) -> Result<Vec<FragmentInfo>, OverlayError> {
    let mut fragments = Vec::new();
    let root = overlay.root;
    let root_node = overlay.node(root).ok_or(OverlayError::MalformedOverlay)?;
    for &child_id in &root_node.children {
        let child = overlay
            .node(child_id)
            .ok_or(OverlayError::MalformedOverlay)?;
        if !child.name.as_str().starts_with("fragment@") {
            continue;
        }

        let mut overlay_child = None;
        for &grandchild in &child.children {
            let grand = overlay
                .node(grandchild)
                .ok_or(OverlayError::MalformedOverlay)?;
            if grand.name.as_str() == "__overlay__" {
                if overlay_child.is_some() {
                    return Err(OverlayError::MalformedOverlay);
                }
                overlay_child = Some(grandchild);
            }
        }
        let overlay_root = overlay_child.ok_or(OverlayError::MissingFragmentOverlay)?;

        let target_prop = child
            .property("target")
            .map(|p| p.value.as_slice().to_vec());
        let target_path_prop = child
            .property("target-path")
            .map(|p| p.value.as_slice().to_vec());

        let target = match (target_prop, target_path_prop) {
            (Some(phandle_bytes), None) => {
                if phandle_bytes.len() != 4 {
                    return Err(OverlayError::InvalidTargetPhandle);
                }
                let phandle = read_be_u32(&phandle_bytes[..4]);
                FragmentTarget::Phandle(phandle)
            }
            (None, Some(path_bytes)) => {
                let path =
                    decode_first_string(&path_bytes).ok_or(OverlayError::InvalidTargetPath)?;
                FragmentTarget::Path(path.to_string())
            }
            (None, None) => return Err(OverlayError::MissingFragmentTarget),
            (Some(_), Some(_)) => return Err(OverlayError::MalformedOverlay),
        };

        fragments.push(FragmentInfo {
            name: child.name.as_str().to_string(),
            target,
            overlay_root,
        });
    }

    if fragments.is_empty() {
        return Err(OverlayError::MalformedOverlay);
    }
    Ok(fragments)
}

fn read_node_phandle_pair(node: &Node<'_>) -> Result<(Option<u32>, Option<u32>), OverlayError> {
    let mut phandle = None;
    let mut linux_phandle = None;

    for prop in &node.properties {
        let name = prop.name.as_str();
        if name == "phandle" || name == "linux,phandle" {
            let value = prop.value.as_slice();
            if value.len() != 4 {
                return Err(OverlayError::InvalidPhandle);
            }
            let parsed = read_be_u32(&value[..4]);
            if parsed == 0 || parsed == u32::MAX {
                return Err(OverlayError::InvalidPhandle);
            }
            if name == "phandle" {
                phandle = Some(parsed);
            } else {
                linux_phandle = Some(parsed);
            }
        }
    }

    Ok((phandle, linux_phandle))
}

fn unify_phandle(
    phandle: Option<u32>,
    linux_phandle: Option<u32>,
) -> Result<Option<u32>, OverlayError> {
    match (phandle, linux_phandle) {
        (Some(p), Some(lp)) => {
            if p == lp {
                Ok(Some(p))
            } else {
                Err(OverlayError::InvalidPhandle)
            }
        }
        (Some(p), None) => Ok(Some(p)),
        (None, Some(lp)) => Ok(Some(lp)),
        (None, None) => Ok(None),
    }
}

fn build_phandle_index(
    tree: &DeviceTree<'_>,
) -> Result<(BTreeMap<u32, NodeId>, u32), OverlayError> {
    let mut index = BTreeMap::new();
    let mut max_phandle = 0u32;

    for (id, node) in tree.nodes.iter().enumerate() {
        let (phandle, linux_phandle) = read_node_phandle_pair(node)?;
        if let Some(p) = unify_phandle(phandle, linux_phandle)? {
            match index.insert(p, id) {
                Some(existing) if existing != id => return Err(OverlayError::DuplicatePhandle),
                _ => {}
            }
            if p > max_phandle {
                max_phandle = p;
            }
        }
    }

    Ok((index, max_phandle))
}

fn adjust_overlay_phandles(overlay: &mut DeviceTree<'_>, delta: u32) -> Result<(), OverlayError> {
    for node in &mut overlay.nodes {
        let (phandle, linux_phandle) = read_node_phandle_pair(node)?;
        let unified = unify_phandle(phandle, linux_phandle)?;
        let Some(old) = unified else {
            continue;
        };
        let new_val = old
            .checked_add(delta)
            .ok_or(OverlayError::PhandleOverflow)?;
        if new_val == u32::MAX {
            return Err(OverlayError::PhandleOverflow);
        }
        if phandle.is_some() {
            node.set_property(
                NameRef::Owned("phandle".to_string()),
                ValueRef::Owned(new_val.to_be_bytes().to_vec()),
            );
        }
        if linux_phandle.is_some() {
            node.set_property(
                NameRef::Owned("linux,phandle".to_string()),
                ValueRef::Owned(new_val.to_be_bytes().to_vec()),
            );
        }
    }
    Ok(())
}

fn apply_local_fixups(overlay: &mut DeviceTree<'_>, delta: u32) -> Result<(), OverlayError> {
    let fixup_root = match overlay.find_node_by_path("/__local_fixups__") {
        Some(id) => id,
        None => return Ok(()),
    };
    let overlay_root = overlay.root;

    apply_local_fixups_recursive(overlay, fixup_root, overlay_root, delta)
}

fn apply_local_fixups_recursive(
    overlay: &mut DeviceTree<'_>,
    fixup_node: NodeId,
    overlay_node: NodeId,
    delta: u32,
) -> Result<(), OverlayError> {
    let (fix_properties, fix_children) = {
        let fix = overlay
            .node(fixup_node)
            .ok_or(OverlayError::LocalFixupMissingNode)?;
        (fix.properties.clone(), fix.children.clone())
    };

    for prop in fix_properties {
        let offsets = parse_u32_list(prop.value.as_slice())?;
        let mut value = {
            let real = overlay
                .node(overlay_node)
                .ok_or(OverlayError::LocalFixupMissingNode)?;
            let target_prop = real
                .property(prop.name.as_str())
                .ok_or(OverlayError::LocalFixupMissingProperty)?;
            target_prop.value.as_slice().to_vec()
        };

        for offset in offsets {
            let start = offset as usize;
            let end = start
                .checked_add(4)
                .ok_or(OverlayError::LocalFixupOffsetOutOfBounds)?;
            if end > value.len() {
                return Err(OverlayError::LocalFixupOffsetOutOfBounds);
            }
            let current = read_be_u32(&value[start..end]);
            let patched = current
                .checked_add(delta)
                .ok_or(OverlayError::PhandleOverflow)?;
            if patched == u32::MAX {
                return Err(OverlayError::PhandleOverflow);
            }
            value[start..end].copy_from_slice(&patched.to_be_bytes());
        }

        overlay
            .node_mut(overlay_node)
            .ok_or(OverlayError::LocalFixupMissingNode)?
            .set_property(
                NameRef::Owned(prop.name.as_str().to_string()),
                ValueRef::Owned(value),
            );
    }

    for child in fix_children {
        let child_name = overlay
            .node(child)
            .ok_or(OverlayError::LocalFixupMissingNode)?
            .name
            .as_str()
            .to_string();
        let real_child = find_child_by_name(overlay, overlay_node, &child_name)
            .ok_or(OverlayError::LocalFixupMissingNode)?;
        apply_local_fixups_recursive(overlay, child, real_child, delta)?;
    }

    Ok(())
}

fn apply_external_fixups(
    base: &DeviceTree<'_>,
    overlay: &mut DeviceTree<'_>,
) -> Result<(), OverlayError> {
    let fixups_root = match overlay.find_node_by_path("/__fixups__") {
        Some(id) => id,
        None => return Ok(()),
    };

    let fixup_properties = overlay
        .node(fixups_root)
        .ok_or(OverlayError::MalformedFixup)?
        .properties
        .clone();
    if fixup_properties.is_empty() {
        return Ok(());
    }

    let symbols_root = base
        .find_node_by_path("/__symbols__")
        .ok_or(OverlayError::MissingSymbols)?;

    let symbols = base
        .node(symbols_root)
        .ok_or(OverlayError::MissingSymbols)?;
    let mut symbol_map = BTreeMap::new();
    for prop in &symbols.properties {
        if let Some(path) = decode_first_string(prop.value.as_slice()) {
            symbol_map.insert(prop.name.as_str().to_string(), path.to_string());
        }
    }

    for prop in fixup_properties {
        let label = prop.name.as_str();
        let target_path = symbol_map.get(label).ok_or(OverlayError::SymbolNotFound)?;
        let base_target = base
            .find_node_by_path(target_path)
            .ok_or(OverlayError::TargetNotFound)?;
        let phandle = read_phandle(base, base_target)?;

        let entries =
            parse_stringlist(prop.value.as_slice()).map_err(|_| OverlayError::MalformedFixup)?;
        for entry in entries {
            let (path_part, prop_name, offset_str) = split_fixup_entry(entry)?;
            let offset: usize = offset_str
                .parse()
                .map_err(|_| OverlayError::MalformedFixup)?;
            let overlay_path = normalize_overlay_path(path_part);
            let target_node = overlay
                .find_node_by_path(&overlay_path)
                .ok_or(OverlayError::FixupPropertyMissing)?;
            let target = overlay
                .node(target_node)
                .ok_or(OverlayError::FixupPropertyMissing)?;
            let prop_target = target
                .property(prop_name)
                .ok_or(OverlayError::FixupPropertyMissing)?;
            let end = offset
                .checked_add(4)
                .ok_or(OverlayError::FixupOffsetOutOfBounds)?;
            if end > prop_target.value.as_slice().len() {
                return Err(OverlayError::FixupOffsetOutOfBounds);
            }

            let mut value = prop_target.value.as_slice().to_vec();
            value[offset..end].copy_from_slice(&phandle.to_be_bytes());
            overlay
                .node_mut(target_node)
                .ok_or(OverlayError::FixupPropertyMissing)?
                .set_property(
                    NameRef::Owned(prop_name.to_string()),
                    ValueRef::Owned(value),
                );
        }
    }

    Ok(())
}

fn merge_overlay_subtree(
    base: &mut DeviceTree<'_>,
    overlay: &DeviceTree<'_>,
    overlay_node: NodeId,
    base_node: NodeId,
    opt: &OverlayApplyOptions,
    report: &mut OverlayApplyReport,
) -> Result<(), OverlayError> {
    let overlay_node_ref = overlay
        .node(overlay_node)
        .ok_or(OverlayError::MalformedOverlay)?;

    if has_delete_directives(overlay_node_ref) && !opt.allow_custom_deletes {
        return Err(OverlayError::DeletesNotAllowed);
    }

    let mut modifications = false;
    let deletes = if opt.allow_custom_deletes {
        apply_delete_directives(base, base_node, overlay_node_ref, opt.strict, report)?
    } else {
        (0, 0)
    };
    if deletes.0 + deletes.1 > 0 {
        modifications = true;
    }

    for prop in &overlay_node_ref.properties {
        let name = prop.name.as_str();
        if is_delete_property(name) {
            continue;
        }
        base.node_mut(base_node)
            .ok_or(OverlayError::MalformedOverlay)?
            .set_property(
                NameRef::Owned(name.to_string()),
                ValueRef::Owned(prop.value.as_slice().to_vec()),
            );
        report.properties_set += 1;
        modifications = true;
    }

    for &child in &overlay_node_ref.children {
        let child_node = overlay.node(child).ok_or(OverlayError::MalformedOverlay)?;
        let child_name = child_node.name.as_str();
        if let Some(existing) = find_child_by_name(base, base_node, child_name) {
            merge_overlay_subtree(base, overlay, child, existing, opt, report)?;
        } else {
            let new_id = copy_subtree(base, overlay, child, base_node, report)?;
            let new_path = node_path(base, new_id);
            report.record_modified_node(new_path);
        }
    }

    if modifications {
        let path = node_path(base, base_node);
        report.record_modified_node(path);
    }

    Ok(())
}

fn apply_delete_directives(
    base: &mut DeviceTree<'_>,
    base_node: NodeId,
    overlay_node: &crate::ast::Node<'_>,
    strict: bool,
    report: &mut OverlayApplyReport,
) -> Result<(usize, usize), OverlayError> {
    let mut props_deleted = 0usize;
    let mut nodes_deleted = 0usize;

    let prop_names = overlay_node
        .property("__delete_properties__")
        .or_else(|| overlay_node.property("delete-property"))
        .map(|p| parse_stringlist(p.value.as_slice()))
        .transpose()
        .map_err(|_| OverlayError::MalformedOverlay)?
        .unwrap_or_default();
    for name in prop_names {
        let removed = base
            .node_mut(base_node)
            .ok_or(OverlayError::MalformedOverlay)?
            .remove_property(name);
        if removed {
            props_deleted += 1;
        } else if strict {
            return Err(OverlayError::StrictViolation);
        }
    }

    let node_names = overlay_node
        .property("__delete_nodes__")
        .or_else(|| overlay_node.property("delete-node"))
        .map(|p| parse_stringlist(p.value.as_slice()))
        .transpose()
        .map_err(|_| OverlayError::MalformedOverlay)?
        .unwrap_or_default();
    for name in node_names {
        if let Some(child_id) = find_child_by_name(base, base_node, name) {
            base.detach_node(child_id)
                .map_err(|_| OverlayError::MalformedOverlay)?;
            nodes_deleted += 1;
        } else if strict {
            return Err(OverlayError::StrictViolation);
        }
    }

    report.properties_deleted += props_deleted;
    report.nodes_deleted += nodes_deleted;
    Ok((props_deleted, nodes_deleted))
}

fn has_delete_directives(node: &crate::ast::Node<'_>) -> bool {
    node.property("__delete_properties__")
        .or_else(|| node.property("delete-property"))
        .is_some()
        || node
            .property("__delete_nodes__")
            .or_else(|| node.property("delete-node"))
            .is_some()
}

fn is_delete_property(name: &str) -> bool {
    matches!(
        name,
        "__delete_properties__" | "__delete_nodes__" | "delete-property" | "delete-node"
    )
}

fn copy_subtree(
    base: &mut DeviceTree<'_>,
    overlay: &DeviceTree<'_>,
    overlay_node: NodeId,
    base_parent: NodeId,
    report: &mut OverlayApplyReport,
) -> Result<NodeId, OverlayError> {
    let overlay_node_ref = overlay
        .node(overlay_node)
        .ok_or(OverlayError::MalformedOverlay)?;
    let new_id = base
        .add_child(
            base_parent,
            NameRef::Owned(overlay_node_ref.name.as_str().to_string()),
        )
        .map_err(|_| OverlayError::MalformedOverlay)?;
    report.nodes_added += 1;

    let mut props_added = 0usize;
    for prop in &overlay_node_ref.properties {
        let name = prop.name.as_str();
        if is_delete_property(name) {
            continue;
        }
        base.node_mut(new_id)
            .ok_or(OverlayError::MalformedOverlay)?
            .set_property(
                NameRef::Owned(name.to_string()),
                ValueRef::Owned(prop.value.as_slice().to_vec()),
            );
        props_added += 1;
    }
    report.properties_set += props_added;

    for &child in &overlay_node_ref.children {
        copy_subtree(base, overlay, child, new_id, report)?;
    }
    Ok(new_id)
}

fn resolve_fragment_target(
    base: &DeviceTree<'_>,
    phandles: &BTreeMap<u32, NodeId>,
    fragment: &FragmentInfo,
) -> Result<NodeId, OverlayError> {
    match &fragment.target {
        FragmentTarget::Phandle(p) => phandles.get(p).copied().ok_or(OverlayError::TargetNotFound),
        FragmentTarget::Path(path) => base
            .find_node_by_path(path)
            .ok_or(OverlayError::TargetNotFound),
    }
}

fn read_phandle(base: &DeviceTree<'_>, node: NodeId) -> Result<u32, OverlayError> {
    let node_ref = base.node(node).ok_or(OverlayError::InvalidPhandle)?;
    let (phandle, linux_phandle) = read_node_phandle_pair(node_ref)?;
    let unified = unify_phandle(phandle, linux_phandle)?;
    unified.ok_or(OverlayError::InvalidPhandle)
}

fn parse_u32_list(bytes: &[u8]) -> Result<Vec<u32>, OverlayError> {
    if bytes.len() % 4 != 0 {
        return Err(OverlayError::MalformedFixup);
    }
    Ok(bytes.chunks_exact(4).map(read_be_u32).collect())
}

fn decode_first_string(bytes: &[u8]) -> Option<&str> {
    parse_stringlist(bytes).ok()?.first().copied()
}

fn parse_stringlist(bytes: &[u8]) -> Result<Vec<&str>, OverlayError> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| core::str::from_utf8(entry).map_err(|_| OverlayError::MalformedOverlay))
        .collect()
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    let b = [bytes[0], bytes[1], bytes[2], bytes[3]];
    u32::from_be_bytes(b)
}

fn split_fixup_entry(entry: &str) -> Result<(&str, &str, &str), OverlayError> {
    let mut parts = entry.splitn(3, ':');
    let path = parts.next().ok_or(OverlayError::MalformedFixup)?;
    let prop = parts.next().ok_or(OverlayError::MalformedFixup)?;
    let offset = parts.next().ok_or(OverlayError::MalformedFixup)?;
    Ok((path, prop, offset))
}

fn normalize_overlay_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        let mut s = String::from("/");
        s.push_str(path);
        s
    }
}

fn find_child_by_name(tree: &DeviceTree<'_>, parent: NodeId, name: &str) -> Option<NodeId> {
    let node = tree.node(parent)?;
    for &child in &node.children {
        let child_node = tree.node(child)?;
        if child_node.name.as_str() == name {
            return Some(child);
        }
    }
    None
}

fn node_path(tree: &DeviceTree<'_>, node_id: NodeId) -> String {
    if node_id == tree.root {
        return "/".to_string();
    }
    let mut parts = Vec::new();
    let mut current = Some(node_id);
    while let Some(id) = current {
        if id == tree.root {
            break;
        }
        if let Some(node) = tree.node(id) {
            parts.push(node.name.as_str().to_string());
            current = node.parent;
        } else {
            break;
        }
    }
    let mut path = String::from("/");
    for (i, part) in parts.iter().rev().enumerate() {
        if i > 0 || path != "/" {
            path.push('/');
        }
        path.push_str(part);
    }
    if path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    path
}

fn update_symbols(
    base: &mut DeviceTree<'_>,
    overlay: &DeviceTree<'_>,
    fragments: &[FragmentInfo],
    report: &mut OverlayApplyReport,
) -> Result<(), OverlayError> {
    let overlay_symbols = match overlay.find_node_by_path("/__symbols__") {
        Some(id) => id,
        None => return Ok(()),
    };

    let mut fragment_map: BTreeMap<&str, &FragmentTarget> = BTreeMap::new();
    for fragment in fragments {
        fragment_map.insert(fragment.name.as_str(), &fragment.target);
    }

    let base_symbols = match base.find_node_by_path("/__symbols__") {
        Some(id) => id,
        None => base
            .add_child(base.root, NameRef::Owned("__symbols__".to_string()))
            .map_err(|_| OverlayError::MalformedOverlay)?,
    };

    let (phandles, _) = build_phandle_index(base)?;

    let overlay_symbols_node = overlay
        .node(overlay_symbols)
        .ok_or(OverlayError::MalformedOverlay)?;

    for prop in &overlay_symbols_node.properties {
        let path = match decode_first_string(prop.value.as_slice()) {
            Some(p) => p,
            None => continue,
        };
        let path_str = normalize_overlay_path(path);
        let mut segments = path_str.split('/').filter(|s| !s.is_empty());
        let fragment_seg = segments.next().ok_or(OverlayError::MalformedOverlay)?;
        let fragment_target = match fragment_map.get(fragment_seg) {
            Some(t) => t,
            None => return Err(OverlayError::MalformedOverlay),
        };
        let base_prefix = match fragment_target {
            FragmentTarget::Phandle(p) => {
                let target_node = phandles
                    .get(p)
                    .copied()
                    .ok_or(OverlayError::TargetNotFound)?;
                node_path(base, target_node)
            }
            FragmentTarget::Path(p) => {
                if p.starts_with('/') {
                    p.clone()
                } else {
                    format!("/{}", p)
                }
            }
        };

        let mut real_path = base_prefix;
        for seg in segments {
            if seg == "__overlay__" {
                continue;
            }
            if !real_path.ends_with('/') && real_path != "/" {
                real_path.push('/');
            }
            real_path.push_str(seg);
        }
        if !real_path.starts_with('/') {
            real_path.insert(0, '/');
        }
        if !real_path.ends_with('\0') {
            real_path.push('\0');
        }
        base.node_mut(base_symbols)
            .ok_or(OverlayError::MalformedOverlay)?
            .set_property(
                NameRef::Owned(prop.name.as_str().to_string()),
                ValueRef::Owned(real_path.as_bytes().to_vec()),
            );
        report.symbols_updated += 1;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::NodeQueryExt;

    fn be32(v: u32) -> Vec<u8> {
        v.to_be_bytes().to_vec()
    }

    fn string_prop(s: &str) -> Vec<u8> {
        let mut v = Vec::from(s.as_bytes());
        if !v.ends_with(&[0]) {
            v.push(0);
        }
        v
    }

    fn new_tree() -> DeviceTree<'static> {
        DeviceTree::with_root(NameRef::Owned("/".to_string()))
    }

    #[test]
    fn build_phandle_index_allows_phandle_and_linux_phandle_same_node() {
        let mut tree = new_tree();
        let node = tree
            .add_child(tree.root, NameRef::Owned("n".to_string()))
            .unwrap();
        tree.node_mut(node).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );
        tree.node_mut(node).unwrap().set_property(
            NameRef::Owned("linux,phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );

        let (index, max) = build_phandle_index(&tree).expect("phandle index");
        assert_eq!(max, 0x10);
        assert_eq!(index.get(&0x10), Some(&node));
    }

    #[test]
    fn build_phandle_index_rejects_mismatched_phandles() {
        let mut tree = new_tree();
        let node = tree
            .add_child(tree.root, NameRef::Owned("n".to_string()))
            .unwrap();
        tree.node_mut(node).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );
        tree.node_mut(node).unwrap().set_property(
            NameRef::Owned("linux,phandle".to_string()),
            ValueRef::Owned(be32(0x11)),
        );

        let err = build_phandle_index(&tree).unwrap_err();
        assert_eq!(err, OverlayError::InvalidPhandle);
    }

    #[test]
    fn basic_merge_updates_property() {
        let mut base = new_tree();
        let soc = base
            .add_child(base.root, NameRef::Owned("soc".to_string()))
            .unwrap();
        let serial = base
            .add_child(soc, NameRef::Owned("serial@0".to_string()))
            .unwrap();
        base.node_mut(serial).unwrap().set_property(
            NameRef::Owned("status".to_string()),
            ValueRef::Owned(string_prop("disabled")),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/soc/serial@0")),
        );
        let overlay_node = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(overlay_node).unwrap().set_property(
            NameRef::Owned("status".to_string()),
            ValueRef::Owned(string_prop("okay")),
        );
        overlay.node_mut(overlay_node).unwrap().set_property(
            NameRef::Owned("baudrate".to_string()),
            ValueRef::Owned(be32(115200)),
        );

        let report = base
            .apply_overlay(&overlay, OverlayApplyOptions::default())
            .expect("apply overlay");
        assert_eq!(report.fragments_applied, 1);
        let serial_node = base.node(serial).unwrap();
        assert_eq!(
            serial_node.property("status").unwrap().value.as_slice(),
            string_prop("okay").as_slice()
        );
        assert_eq!(
            serial_node.property("baudrate").unwrap().value.as_slice(),
            be32(115200).as_slice()
        );

        let dtb = base.clone().into_dtb_box().expect("serialize dtb");
        let reparsed = DeviceTree::from_dtb(&dtb).expect("reparse dtb");
        let reparsed_serial = reparsed
            .find_node_by_path("/soc/serial@0")
            .expect("serial present");
        let reparsed_node = reparsed.node(reparsed_serial).unwrap();
        assert_eq!(
            reparsed_node.property("status").unwrap().value.as_slice(),
            string_prop("okay").as_slice()
        );
    }

    #[test]
    fn subtree_addition_creates_new_node() {
        let mut base = new_tree();
        let soc = base
            .add_child(base.root, NameRef::Owned("soc".to_string()))
            .unwrap();

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/soc")),
        );
        let overlay_node = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let added = overlay
            .add_child(overlay_node, NameRef::Owned("newnode".to_string()))
            .unwrap();
        overlay.node_mut(added).unwrap().set_property(
            NameRef::Owned("answer".to_string()),
            ValueRef::Owned(be32(42)),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .unwrap();

        let new_id = find_child_by_name(&base, soc, "newnode").expect("new node exists");
        let node = base.node(new_id).unwrap();
        assert_eq!(
            node.property("answer").unwrap().value.as_slice(),
            be32(42).as_slice()
        );
    }

    #[test]
    fn local_phandle_fixups_shift_references() {
        let mut base = new_tree();
        let soc = base
            .add_child(base.root, NameRef::Owned("soc".to_string()))
            .unwrap();
        base.node_mut(soc).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let newn = overlay
            .add_child(ov, NameRef::Owned("child".to_string()))
            .unwrap();
        overlay.node_mut(newn).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(1)),
        );
        overlay
            .node_mut(newn)
            .unwrap()
            .set_property(NameRef::Owned("ref".to_string()), ValueRef::Owned(be32(1)));

        let fix_root = overlay
            .add_child(overlay.root, NameRef::Owned("__local_fixups__".to_string()))
            .unwrap();
        let fix_frag = overlay
            .add_child(fix_root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        let fix_overlay = overlay
            .add_child(fix_frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let fix_child = overlay
            .add_child(fix_overlay, NameRef::Owned("child".to_string()))
            .unwrap();
        overlay
            .node_mut(fix_child)
            .unwrap()
            .set_property(NameRef::Owned("ref".to_string()), ValueRef::Owned(be32(0)));

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .unwrap();
        let child = find_child_by_name(&base, base.root, "child").unwrap();
        let node = base.node(child).unwrap();
        assert_eq!(
            node.property("ref").unwrap().value.as_slice(),
            be32(0x11).as_slice()
        );
        assert_eq!(
            node.property("phandle").unwrap().value.as_slice(),
            be32(0x11).as_slice()
        );
    }

    #[test]
    fn external_fixups_patch_phandle_from_symbols() {
        let mut base = new_tree();
        let target = base
            .add_child(base.root, NameRef::Owned("tgt".to_string()))
            .unwrap();
        base.node_mut(target).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x20)),
        );
        let symbols = base
            .add_child(base.root, NameRef::Owned("__symbols__".to_string()))
            .unwrap();
        base.node_mut(symbols).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/tgt")),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay
            .node_mut(ov)
            .unwrap()
            .set_property(NameRef::Owned("ref".to_string()), ValueRef::Owned(be32(0)));

        let fixups = overlay
            .add_child(overlay.root, NameRef::Owned("__fixups__".to_string()))
            .unwrap();
        overlay.node_mut(fixups).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/fragment@0/__overlay__:ref:0")),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .unwrap();
        let root = base.node(base.root).unwrap();
        assert_eq!(
            root.property("ref").unwrap().value.as_slice(),
            be32(0x20).as_slice()
        );
    }

    #[test]
    fn custom_deletes_remove_properties_and_nodes() {
        let mut base = new_tree();
        let parent = base
            .add_child(base.root, NameRef::Owned("p".to_string()))
            .unwrap();
        base.node_mut(parent)
            .unwrap()
            .set_property(NameRef::Owned("keep".to_string()), ValueRef::Owned(be32(1)));
        base.node_mut(parent)
            .unwrap()
            .set_property(NameRef::Owned("drop".to_string()), ValueRef::Owned(be32(2)));
        let child = base
            .add_child(parent, NameRef::Owned("child".to_string()))
            .unwrap();
        base.node_mut(child).unwrap().set_property(
            NameRef::Owned("value".to_string()),
            ValueRef::Owned(be32(3)),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/p")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("__delete_properties__".to_string()),
            ValueRef::Owned(string_prop("drop")),
        );
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("__delete_nodes__".to_string()),
            ValueRef::Owned(string_prop("child")),
        );
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("newprop".to_string()),
            ValueRef::Owned(be32(9)),
        );

        base.apply_overlay(
            &overlay,
            OverlayApplyOptions {
                allow_custom_deletes: true,
                ..Default::default()
            },
        )
        .unwrap();
        let parent_node = base.node(parent).unwrap();
        assert!(parent_node.property("drop").is_none());
        assert!(find_child_by_name(&base, parent, "child").is_none());
        assert!(parent_node.property("newprop").is_some());
    }

    #[test]
    fn strict_delete_missing_fails() {
        let mut base = new_tree();
        let parent = base
            .add_child(base.root, NameRef::Owned("p".to_string()))
            .unwrap();

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/p")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("__delete_properties__".to_string()),
            ValueRef::Owned(string_prop("missing")),
        );

        let err = base
            .apply_overlay(
                &overlay,
                OverlayApplyOptions {
                    allow_custom_deletes: true,
                    strict: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(err, OverlayError::StrictViolation);
        assert!(base.node(parent).unwrap().properties.is_empty());
    }

    #[test]
    fn missing_delete_is_ignored_when_not_strict() {
        let mut base = new_tree();
        let parent = base
            .add_child(base.root, NameRef::Owned("p".to_string()))
            .unwrap();
        base.node_mut(parent)
            .unwrap()
            .set_property(NameRef::Owned("keep".to_string()), ValueRef::Owned(be32(1)));

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/p")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("__delete_nodes__".to_string()),
            ValueRef::Owned(string_prop("nope")),
        );

        base.apply_overlay(
            &overlay,
            OverlayApplyOptions {
                allow_custom_deletes: true,
                strict: false,
                ..Default::default()
            },
        )
        .expect("non-strict should ignore missing delete");

        let parent_node = base.node(parent).unwrap();
        assert!(parent_node.property("keep").is_some());
    }

    #[test]
    fn symbols_are_updated_from_overlay() {
        let mut base = new_tree();
        let target = base
            .add_child(base.root, NameRef::Owned("tgt".to_string()))
            .unwrap();

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/tgt")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let child = overlay
            .add_child(ov, NameRef::Owned("child".to_string()))
            .unwrap();
        overlay.node_mut(child).unwrap().set_property(
            NameRef::Owned("value".to_string()),
            ValueRef::Owned(be32(7)),
        );

        let symbols = overlay
            .add_child(overlay.root, NameRef::Owned("__symbols__".to_string()))
            .unwrap();
        overlay.node_mut(symbols).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/fragment@0/__overlay__/child")),
        );

        base.apply_overlay(
            &overlay,
            OverlayApplyOptions {
                update_symbols: true,
                ..Default::default()
            },
        )
        .unwrap();

        let symbols_id = base
            .find_node_by_path("/__symbols__")
            .expect("symbols created");
        let symbols_node = base.node(symbols_id).unwrap();
        assert_eq!(
            symbols_node.property("lab").unwrap().value.as_slice(),
            string_prop("/tgt/child").as_slice()
        );
        assert!(find_child_by_name(&base, target, "child").is_some());
    }

    #[test]
    fn fragment_target_is_patched_before_merge() {
        let mut base = new_tree();
        let tgt = base
            .add_child(base.root, NameRef::Owned("tgt".to_string()))
            .unwrap();
        base.node_mut(tgt).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x20)),
        );
        let symbols = base
            .add_child(base.root, NameRef::Owned("__symbols__".to_string()))
            .unwrap();
        base.node_mut(symbols).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/tgt")),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target".to_string()),
            ValueRef::Owned(vec![0xff, 0xff, 0xff, 0xff]),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("from-overlay".to_string()),
            ValueRef::Owned(be32(1)),
        );
        let fixups = overlay
            .add_child(overlay.root, NameRef::Owned("__fixups__".to_string()))
            .unwrap();
        overlay.node_mut(fixups).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/fragment@0:target:0")),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .expect("apply overlay");

        let tgt_node = base.node(tgt).unwrap();
        assert_eq!(
            tgt_node.property("from-overlay").unwrap().value.as_slice(),
            be32(1).as_slice()
        );
    }

    #[test]
    fn external_fixup_accepts_unaligned_offset() {
        let mut base = new_tree();
        let target = base
            .add_child(base.root, NameRef::Owned("tgt".to_string()))
            .unwrap();
        base.node_mut(target).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x20)),
        );
        let symbols = base
            .add_child(base.root, NameRef::Owned("__symbols__".to_string()))
            .unwrap();
        base.node_mut(symbols).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/tgt")),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay.node_mut(ov).unwrap().set_property(
            NameRef::Owned("refbytes".to_string()),
            ValueRef::Owned(vec![0xAA, 0x00, 0x00, 0x00, 0x00]),
        );
        let fixups = overlay
            .add_child(overlay.root, NameRef::Owned("__fixups__".to_string()))
            .unwrap();
        overlay.node_mut(fixups).unwrap().set_property(
            NameRef::Owned("lab".to_string()),
            ValueRef::Owned(string_prop("/fragment@0/__overlay__:refbytes:1")),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .expect("apply overlay");

        let root = base.node(base.root).unwrap();
        let bytes = root.property("refbytes").unwrap().value.as_slice();
        assert_eq!(&bytes[1..5], be32(0x20).as_slice());
    }

    #[test]
    fn local_fixup_accepts_unaligned_offset() {
        let mut base = new_tree();
        let _existing = base
            .add_child(base.root, NameRef::Owned("ph".to_string()))
            .unwrap();
        base.node_mut(base.root).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let res = overlay
            .add_child(ov, NameRef::Owned("res".to_string()))
            .unwrap();
        overlay.node_mut(res).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(1)),
        );
        overlay.node_mut(res).unwrap().set_property(
            NameRef::Owned("refbytes".to_string()),
            ValueRef::Owned(vec![0xBB, 0x00, 0x00, 0x00, 0x01]),
        );

        let fix_root = overlay
            .add_child(overlay.root, NameRef::Owned("__local_fixups__".to_string()))
            .unwrap();
        let fix_frag = overlay
            .add_child(fix_root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        let fix_ov = overlay
            .add_child(fix_frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let fix_res = overlay
            .add_child(fix_ov, NameRef::Owned("res".to_string()))
            .unwrap();
        overlay.node_mut(fix_res).unwrap().set_property(
            NameRef::Owned("refbytes".to_string()),
            ValueRef::Owned(be32(1)),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .expect("apply overlay");

        let res_node = find_child_by_name(&base, base.root, "res").expect("res exists");
        let bytes = base
            .node(res_node)
            .unwrap()
            .property("refbytes")
            .unwrap()
            .value
            .as_slice();
        assert_eq!(&bytes[1..5], be32(0x11).as_slice());
    }

    #[test]
    fn local_fixup_allows_empty_root_name() {
        let mut base = DeviceTree::with_root(NameRef::Owned(String::new()));
        base.node_mut(base.root).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(0x10)),
        );

        let mut overlay = DeviceTree::with_root(NameRef::Owned(String::new()));
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let res = overlay
            .add_child(ov, NameRef::Owned("res".to_string()))
            .unwrap();
        overlay.node_mut(res).unwrap().set_property(
            NameRef::Owned("phandle".to_string()),
            ValueRef::Owned(be32(1)),
        );
        overlay.node_mut(res).unwrap().set_property(
            NameRef::Owned("refbytes".to_string()),
            ValueRef::Owned(vec![0xAA, 0x00, 0x00, 0x00, 0x01]),
        );

        let fix_root = overlay
            .add_child(overlay.root, NameRef::Owned("__local_fixups__".to_string()))
            .unwrap();
        let fix_frag = overlay
            .add_child(fix_root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        let fix_ov = overlay
            .add_child(fix_frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        let fix_res = overlay
            .add_child(fix_ov, NameRef::Owned("res".to_string()))
            .unwrap();
        overlay.node_mut(fix_res).unwrap().set_property(
            NameRef::Owned("refbytes".to_string()),
            ValueRef::Owned(be32(1)),
        );

        base.apply_overlay(&overlay, OverlayApplyOptions::default())
            .expect("apply overlay");

        let res_node = find_child_by_name(&base, base.root, "res").expect("res exists");
        let bytes = base
            .node(res_node)
            .unwrap()
            .property("refbytes")
            .unwrap()
            .value
            .as_slice();
        assert_eq!(&bytes[1..5], be32(0x11).as_slice());
    }

    #[test]
    fn empty_fixups_node_is_noop_even_without_base_symbols() {
        let mut base = new_tree();

        let mut overlay = new_tree();
        let frag = overlay
            .add_child(overlay.root, NameRef::Owned("fragment@0".to_string()))
            .unwrap();
        overlay.node_mut(frag).unwrap().set_property(
            NameRef::Owned("target-path".to_string()),
            ValueRef::Owned(string_prop("/")),
        );
        let ov = overlay
            .add_child(frag, NameRef::Owned("__overlay__".to_string()))
            .unwrap();
        overlay
            .node_mut(ov)
            .unwrap()
            .set_property(NameRef::Owned("x".to_string()), ValueRef::Owned(be32(1)));
        overlay
            .add_child(overlay.root, NameRef::Owned("__fixups__".to_string()))
            .unwrap();

        base.apply_overlay(
            &overlay,
            OverlayApplyOptions {
                strict: true,
                ..Default::default()
            },
        )
        .expect("empty fixups should be no-op");

        let root = base.node(base.root).unwrap();
        assert_eq!(root.property("x").unwrap().value.as_slice(), be32(1));
    }
}
