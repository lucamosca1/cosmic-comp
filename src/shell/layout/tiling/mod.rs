// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    shell::{
        element::{CosmicMapped, CosmicMappedRenderElement},
        focus::{
            target::{KeyboardFocusTarget, WindowGroup},
            FocusDirection,
        },
        layout::Orientation,
        OutputNotMapped,
    },
    utils::prelude::*,
};

use id_tree::{InsertBehavior, MoveBehavior, Node, NodeId, NodeIdError, RemoveBehavior, Tree};
use smithay::{
    backend::renderer::{element::AsRenderElements, ImportAll, Renderer},
    desktop::{layer_map_for_output, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    output::{Output, WeakOutput},
    render_elements,
    utils::{IsAlive, Logical, Point, Rectangle, Scale, Serial},
};
use std::{
    borrow::Borrow,
    cell::RefCell,
    collections::{HashMap, VecDeque},
    hash::Hash,
    sync::{atomic::AtomicBool, Arc},
};

/*
mod grabs;
pub use self::grabs::*;
*/

#[derive(Debug, Clone)]
struct OutputData {
    output: Output,
    location: Point<i32, Logical>,
}

impl Borrow<Output> for OutputData {
    fn borrow(&self) -> &Output {
        &self.output
    }
}

impl PartialEq for OutputData {
    fn eq(&self, other: &Self) -> bool {
        self.output == other.output
    }
}

impl Eq for OutputData {}

impl PartialEq<Output> for OutputData {
    fn eq(&self, other: &Output) -> bool {
        &self.output == other
    }
}

impl Hash for OutputData {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.output.hash(state)
    }
}

#[derive(Debug, Clone)]
pub struct TilingLayout {
    gaps: (i32, i32),
    trees: HashMap<OutputData, Tree<Data>>,
}

#[derive(Debug, Clone)]
pub enum Data {
    Group {
        orientation: Orientation,
        sizes: Vec<i32>,
        last_geometry: Rectangle<i32, Logical>,
        alive: Arc<()>,
    },
    Mapped {
        mapped: CosmicMapped,
        last_geometry: Rectangle<i32, Logical>,
    },
}

impl Data {
    fn new_group(orientation: Orientation, geo: Rectangle<i32, Logical>) -> Data {
        Data::Group {
            orientation,
            sizes: vec![
                match orientation {
                    Orientation::Vertical => geo.size.w / 2,
                    Orientation::Horizontal => geo.size.h / 2,
                };
                2
            ],
            last_geometry: geo,
            alive: Arc::new(()),
        }
    }

    fn is_group(&self) -> bool {
        matches!(self, Data::Group { .. })
    }
    fn is_mapped(&self, mapped: Option<&CosmicMapped>) -> bool {
        match mapped {
            Some(m) => matches!(self, Data::Mapped { mapped, .. } if m == mapped),
            None => matches!(self, Data::Mapped { .. }),
        }
    }

    fn orientation(&self) -> Orientation {
        match self {
            Data::Group { orientation, .. } => *orientation,
            _ => panic!("Not a group"),
        }
    }

    fn add_window(&mut self, idx: usize) {
        match self {
            Data::Group {
                sizes,
                last_geometry,
                orientation,
                ..
            } => {
                let last_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let equal_sizing = last_length / (sizes.len() as i32 + 1); // new window size
                let remainder = last_length - equal_sizing; // size for the rest of the windowns

                for size in sizes.iter_mut() {
                    *size = ((*size as f64 / last_length as f64) * remainder as f64).round() as i32;
                }
                let used_size: i32 = sizes.iter().sum();
                let new_size = last_length - used_size;

                sizes.insert(idx, new_size);
            }
            Data::Mapped { .. } => panic!("Adding window to leaf?"),
        }
    }

    fn remove_window(&mut self, idx: usize) {
        match self {
            Data::Group {
                sizes,
                last_geometry,
                orientation,
                ..
            } => {
                let last_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let old_size = sizes.remove(idx);
                for size in sizes.iter_mut() {
                    *size +=
                        ((old_size as f64 / last_length as f64) * (*size as f64)).round() as i32;
                }
                let used_size: i32 = sizes.iter().sum();
                let overflow = last_length - used_size;
                if overflow != 0 {
                    *sizes.last_mut().unwrap() += overflow;
                }
            }
            Data::Mapped { .. } => panic!("Added window to leaf?"),
        }
    }

    fn geometry(&self) -> &Rectangle<i32, Logical> {
        match self {
            Data::Group { last_geometry, .. } => last_geometry,
            Data::Mapped { last_geometry, .. } => last_geometry,
        }
    }

    fn update_geometry(&mut self, geo: Rectangle<i32, Logical>) {
        match self {
            Data::Group {
                orientation,
                sizes,
                last_geometry,
                ..
            } => {
                let previous_length = match orientation {
                    Orientation::Horizontal => last_geometry.size.h,
                    Orientation::Vertical => last_geometry.size.w,
                };
                let new_length = match orientation {
                    Orientation::Horizontal => geo.size.h,
                    Orientation::Vertical => geo.size.w,
                };

                sizes.iter_mut().for_each(|len| {
                    *len = (((*len as f64) / (previous_length as f64)) * (new_length as f64))
                        .round() as i32;
                });
                let sum: i32 = sizes.iter().sum();
                if sum < new_length {
                    *sizes.last_mut().unwrap() += new_length - sum;
                }
                *last_geometry = geo;
            }
            Data::Mapped { last_geometry, .. } => {
                *last_geometry = geo;
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Data::Group { sizes, .. } => sizes.len(),
            Data::Mapped { .. } => 1,
        }
    }
}

impl TilingLayout {
    pub fn new() -> TilingLayout {
        TilingLayout {
            gaps: (0, 4),
            trees: HashMap::new(),
        }
    }
}

impl TilingLayout {
    pub fn map_output(&mut self, output: &Output, location: Point<i32, Logical>) {
        if !self.trees.contains_key(output) {
            self.trees.insert(
                OutputData {
                    output: output.clone(),
                    location,
                },
                Tree::new(),
            );
        } else {
            let tree = self.trees.remove(output).unwrap();
            self.trees.insert(
                OutputData {
                    output: output.clone(),
                    location,
                },
                tree,
            );
        }
    }

    pub fn unmap_output(&mut self, output: &Output) {
        if let Some(src) = self.trees.remove(output) {
            // TODO: expects last remaining output
            let (output, dst) = self.trees.iter_mut().next().unwrap();
            let orientation = match output.output.geometry().size {
                x if x.w >= x.h => Orientation::Horizontal,
                _ => Orientation::Vertical,
            };
            TilingLayout::merge_trees(src, dst, orientation);
            self.refresh()
        }
    }

    pub fn map<'a>(
        &mut self,
        window: CosmicMapped,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a CosmicMapped> + 'a,
    ) {
        let output = seat.active_output();
        self.map_internal(window, &output, Some(focus_stack));
        self.refresh();
    }

    fn map_internal<'a>(
        &mut self,
        window: impl Into<CosmicMapped>,
        output: &Output,
        focus_stack: Option<impl Iterator<Item = &'a CosmicMapped> + 'a>,
    ) {
        let tree = self.trees.get_mut(output).expect("Output not mapped?");
        let window = window.into();
        let new_window = Node::new(Data::Mapped {
            mapped: window.clone(),
            last_geometry: Rectangle::from_loc_and_size((0, 0), (100, 100)),
        });

        let last_active =
            focus_stack.and_then(|focus_stack| TilingLayout::last_active_window(tree, focus_stack));

        let window_id = if let Some((_last_active_window, ref node_id)) = last_active {
            let orientation = {
                let window_size = tree.get(node_id).unwrap().data().geometry().size;
                if window_size.w > window_size.h {
                    Orientation::Vertical
                } else {
                    Orientation::Horizontal
                }
            };
            TilingLayout::new_group(tree, &node_id, new_window, orientation)
        } else {
            // nothing? then we add to the root
            if let Some(root_id) = tree.root_node_id().cloned() {
                let orientation = {
                    let output_size = output.geometry().size;
                    if output_size.w > output_size.h {
                        Orientation::Vertical
                    } else {
                        Orientation::Horizontal
                    }
                };
                TilingLayout::new_group(tree, &root_id, new_window, orientation)
            } else {
                tree.insert(new_window, InsertBehavior::AsRoot)
            }
        }
        .unwrap();

        *window.tiling_node_id.lock().unwrap() = Some(window_id);
    }

    pub fn unmap(&mut self, window: &CosmicMapped) -> bool {
        if self.unmap_window_internal(window) {
            window.set_tiled(false);
            self.refresh();
            true
        } else {
            false
        }
    }

    fn unmap_window_internal(&mut self, mapped: &CosmicMapped) -> bool {
        if let Some(node_id) = mapped.tiling_node_id.lock().unwrap().as_ref() {
            if let Some(tree) = self.trees.values_mut().find(|tree| {
                tree.get(node_id)
                    .map(|node| node.data().is_mapped(Some(mapped)))
                    .unwrap_or(false)
            }) {
                let parent_id = tree
                    .get(&node_id)
                    .ok()
                    .and_then(|node| node.parent())
                    .cloned();
                let position = parent_id.as_ref().and_then(|parent_id| {
                    tree.children_ids(&parent_id)
                        .unwrap()
                        .position(|id| id == node_id)
                });
                let parent_parent_id = parent_id.as_ref().and_then(|parent_id| {
                    tree.get(parent_id)
                        .ok()
                        .and_then(|node| node.parent())
                        .cloned()
                });

                // remove self
                slog_scope::debug!("Remove window {:?}", mapped);
                let _ = tree.remove_node(node_id.clone(), RemoveBehavior::DropChildren);

                // fixup parent node
                match parent_id {
                    Some(id) => {
                        let position = position.unwrap();
                        let group = tree.get_mut(&id).unwrap().data_mut();
                        assert!(group.is_group());

                        if group.len() > 2 {
                            group.remove_window(position);
                        } else {
                            slog_scope::debug!("Removing Group");
                            let other_child =
                                tree.children_ids(&id).unwrap().cloned().next().unwrap();
                            let fork_pos = parent_parent_id.as_ref().and_then(|parent_id| {
                                tree.children_ids(parent_id).unwrap().position(|i| i == &id)
                            });
                            let _ = tree.remove_node(id.clone(), RemoveBehavior::OrphanChildren);
                            tree.move_node(
                                &other_child,
                                parent_parent_id
                                    .as_ref()
                                    .map(|parent_id| MoveBehavior::ToParent(parent_id))
                                    .unwrap_or(MoveBehavior::ToRoot),
                            )
                            .unwrap();
                            if let Some(old_pos) = fork_pos {
                                tree.make_nth_sibling(&other_child, old_pos).unwrap();
                            }
                        }
                    }
                    None => {} // root
                }

                return true;
            }
        }
        false
    }

    pub fn output_for_element(&self, elem: &CosmicMapped) -> Option<&Output> {
        self.mapped().find_map(|(o, m, _)| (m == elem).then_some(o))
    }

    pub fn element_geometry(&self, elem: &CosmicMapped) -> Option<Rectangle<i32, Logical>> {
        if let Some(id) = elem.tiling_node_id.lock().unwrap().as_ref() {
            if let Some(output) = self.output_for_element(elem) {
                let (output_data, tree) = self.trees.get_key_value(output).unwrap();
                let node = tree.get(id).ok()?;
                let data = node.data();
                assert!(data.is_mapped(Some(elem)));
                let mut geo = *data.geometry();
                geo.loc += output_data.location;
                return Some(geo);
            }
        }
        None
    }

    pub fn next_focus<'a>(
        &mut self,
        direction: FocusDirection,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a CosmicMapped> + 'a,
    ) -> Option<KeyboardFocusTarget> {
        let output = seat.active_output();
        let tree = self.trees.get_mut(&output).unwrap();

        // TODO: Rather use something like seat.current_keyboard_focus
        // TODO https://github.com/Smithay/smithay/pull/777
        if let Some(last_active) = TilingLayout::last_active_window(tree, focus_stack) {
            let (last_window, node_id) = last_active;

            // stacks may handle focus internally
            if last_window.handle_focus(direction) {
                return None;
            }

            while let Some(group) = tree.get(&node_id).unwrap().parent() {
                let child = node_id.clone();
                let group_data = tree.get(&group).unwrap().data();
                let main_orientation = group_data.orientation();
                assert!(group_data.is_group());

                if direction == FocusDirection::Out {
                    return Some(
                        WindowGroup {
                            node: group.clone(),
                            output: output.downgrade(),
                            alive: match group_data {
                                &Data::Group { ref alive, .. } => Arc::downgrade(alive),
                                _ => unreachable!(),
                            },
                        }
                        .into(),
                    );
                }

                // which child are we?
                let idx = tree
                    .children_ids(&group)
                    .unwrap()
                    .position(|id| id == &child)
                    .unwrap();
                let len = group_data.len();

                let focus_subtree = match (main_orientation, direction) {
                    (Orientation::Horizontal, FocusDirection::Down)
                    | (Orientation::Vertical, FocusDirection::Right)
                        if idx < (len - 1) =>
                    {
                        tree.children_ids(&group).unwrap().skip(idx + 1).next()
                    }
                    (Orientation::Horizontal, FocusDirection::Up)
                    | (Orientation::Vertical, FocusDirection::Left)
                        if idx > 0 =>
                    {
                        tree.children_ids(&group).unwrap().skip(idx - 1).next()
                    }
                    _ => None, // continue iterating
                };

                if focus_subtree.is_some() {
                    let mut node_id = focus_subtree;
                    while node_id.is_some() {
                        match tree.get(node_id.unwrap()).unwrap().data() {
                            Data::Group { orientation, .. } if orientation == &main_orientation => {
                                // if the group is layed out in the direction we care about,
                                // we can just use the first or last element (depending on the direction)
                                match direction {
                                    FocusDirection::Down | FocusDirection::Right => {
                                        node_id = tree
                                            .children_ids(node_id.as_ref().unwrap())
                                            .unwrap()
                                            .next();
                                    }
                                    FocusDirection::Up | FocusDirection::Left => {
                                        node_id = tree
                                            .children_ids(node_id.as_ref().unwrap())
                                            .unwrap()
                                            .last();
                                    }
                                    _ => unreachable!(),
                                }
                            }
                            Data::Group { .. } => {
                                let center = {
                                    let geo = tree.get(&child).unwrap().data().geometry();
                                    let mut point = geo.loc;
                                    match direction {
                                        FocusDirection::Down => {
                                            point += Point::from((geo.size.w / 2, geo.size.h))
                                        }
                                        FocusDirection::Up => point.x += geo.size.w,
                                        FocusDirection::Left => point.y += geo.size.h / 2,
                                        FocusDirection::Right => {
                                            point += Point::from((geo.size.w, geo.size.h / 2))
                                        }
                                        _ => unreachable!(),
                                    };
                                    point.to_f64()
                                };

                                node_id = tree
                                    .children_ids(node_id.as_ref().unwrap())
                                    .unwrap()
                                    .min_by(|node1, node2| {
                                        let distance = |candidate: &&NodeId| -> f64 {
                                            let geo =
                                                tree.get(candidate).unwrap().data().geometry();
                                            let mut point = geo.loc;
                                            match direction {
                                                FocusDirection::Up => {
                                                    point +=
                                                        Point::from((geo.size.w / 2, geo.size.h))
                                                }
                                                FocusDirection::Down => point.x += geo.size.w,
                                                FocusDirection::Right => point.y += geo.size.h / 2,
                                                FocusDirection::Left => {
                                                    point +=
                                                        Point::from((geo.size.w, geo.size.h / 2))
                                                }
                                                _ => unreachable!(),
                                            };
                                            let point = point.to_f64();
                                            ((point.x - center.x).powi(2)
                                                + (point.y - center.y).powi(2))
                                            .sqrt()
                                        };

                                        distance(node1).total_cmp(&distance(node2))
                                    });
                            }
                            Data::Mapped { mapped, .. } => {
                                return Some(mapped.clone().into());
                            }
                        }
                    }
                }
            }
        }

        None
    }

    pub fn update_orientation<'a>(
        &mut self,
        new_orientation: Orientation,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a CosmicMapped> + 'a,
    ) {
        let output = seat.active_output();
        let tree = self.trees.get_mut(&output).unwrap();
        if let Some((_, last_active)) = TilingLayout::last_active_window(tree, focus_stack) {
            if let Some(group) = tree.get(&last_active).unwrap().parent().cloned() {
                if let &mut Data::Group {
                    ref mut orientation,
                    ref mut sizes,
                    ref last_geometry,
                    ..
                } = tree.get_mut(&group).unwrap().data_mut()
                {
                    let previous_length = match orientation {
                        Orientation::Horizontal => last_geometry.size.h,
                        Orientation::Vertical => last_geometry.size.w,
                    };
                    let new_length = match new_orientation {
                        Orientation::Horizontal => last_geometry.size.h,
                        Orientation::Vertical => last_geometry.size.w,
                    };

                    sizes.iter_mut().for_each(|len| {
                        *len = (((*len as f64) / (previous_length as f64)) * (new_length as f64))
                            .round() as i32;
                    });
                    let sum: i32 = sizes.iter().sum();
                    if sum < new_length {
                        *sizes.last_mut().unwrap() += new_length - sum;
                    }

                    *orientation = new_orientation;
                }
            }
        }
        self.refresh();
    }

    pub fn refresh<'a>(&mut self) {
        let dead_windows = self
            .mapped()
            .map(|(_, w, _)| w.clone())
            .filter(|w| !w.alive())
            .collect::<Vec<_>>();
        for dead_window in dead_windows.iter() {
            self.unmap_window_internal(&dead_window);
        }
        TilingLayout::update_space_positions(&mut self.trees, self.gaps);
    }

    /*
    pub fn resize_request(
        window: &CosmicWindow,
        seat: &Seat<State>,
        serial: Serial,
        start_data: PointerGrabStartData<State>,
        edges: ResizeEdge,
    ) {
        // it is so stupid, that we have to do this here. TODO: Refactor grabs
        let workspace = state
            .common
            .shell
            .space_for_window_mut(window.toplevel().wl_surface())
            .unwrap();
        let space = &mut workspace.space;
        let trees = &mut workspace.tiling_layer.trees;

        if let Some(pointer) = seat.get_pointer() {
            if let Some(info) = window.user_data().get::<RefCell<WindowInfo>>() {
                let output = info.borrow().output;
                let tree = TilingLayout::active_tree(trees, output);
                let mut node_id = info.borrow().node.clone();

                while let Some((fork, child)) = TilingLayout::find_fork(tree, node_id) {
                    if let &Data::Fork {
                        ref orientation,
                        ref ratio,
                    } = tree.get(&fork).unwrap().data()
                    {
                        // found a fork
                        // which child are we?
                        let first = tree.children_ids(&fork).unwrap().next() == Some(&child);
                        match (first, orientation, edges) {
                            (true, Orientation::Horizontal, ResizeEdge::Bottom)
                            | (false, Orientation::Horizontal, ResizeEdge::Top)
                            | (true, Orientation::Vertical, ResizeEdge::Right)
                            | (false, Orientation::Vertical, ResizeEdge::Left) => {
                                let output = space.outputs().nth(output).cloned();
                                if let Some(output) = output {
                                    let grab = ResizeForkGrab {
                                        start_data,
                                        orientation: *orientation,
                                        initial_ratio: ratio.load(Ordering::SeqCst),
                                        initial_size: layer_map_for_output(&output)
                                            .non_exclusive_zone()
                                            .size,
                                        ratio: ratio.clone(),
                                    };

                                    pointer.set_grab(state, grab, serial, Focus::Clear);
                                }
                                return;
                            }
                            _ => {} // continue iterating
                        }
                    }
                    node_id = fork;
                }
            }
        }
    }
    */

    fn last_active_window<'a>(
        tree: &mut Tree<Data>,
        mut focus_stack: impl Iterator<Item = &'a CosmicMapped>,
    ) -> Option<(CosmicMapped, NodeId)> {
        focus_stack
            .find_map(|mapped| tree.root_node_id()
                .and_then(|root| tree.traverse_pre_order_ids(root).unwrap()
                    .find(|id| matches!(tree.get(id).map(|n| n.data()), Ok(Data::Mapped { mapped: m, .. }) if m == mapped))
                ).map(|id| (mapped.clone(), id))
            )
    }

    fn new_group(
        tree: &mut Tree<Data>,
        old_id: &NodeId,
        new: Node<Data>,
        orientation: Orientation,
    ) -> Result<NodeId, NodeIdError> {
        let new_group = Node::new(Data::new_group(
            orientation,
            Rectangle::from_loc_and_size((0, 0), (100, 100)),
        ));
        let old = tree.get(old_id)?;
        let parent_id = old.parent().cloned();
        let pos = parent_id.as_ref().and_then(|parent_id| {
            tree.children_ids(parent_id)
                .unwrap()
                .position(|id| id == old_id)
        });

        let group_id = tree
            .insert(
                new_group,
                if let Some(parent) = parent_id.as_ref() {
                    InsertBehavior::UnderNode(parent)
                } else {
                    InsertBehavior::AsRoot
                },
            )
            .unwrap();

        tree.move_node(old_id, MoveBehavior::ToParent(&group_id))
            .unwrap();
        // keep position
        if let Some(old_pos) = pos {
            tree.make_nth_sibling(&group_id, old_pos).unwrap();
        }
        tree.insert(new, InsertBehavior::UnderNode(&group_id))
    }

    fn update_space_positions(trees: &mut HashMap<OutputData, Tree<Data>>, gaps: (i32, i32)) {
        let (outer, inner) = gaps;
        for (output, tree) in trees
            .iter_mut()
            .map(|(output_data, tree)| (&output_data.output, tree))
        {
            if let Some(root) = tree.root_node_id() {
                let mut stack = VecDeque::new();

                let mut geo = Some(layer_map_for_output(&output).non_exclusive_zone());
                // TODO saturate? minimum?
                if let Some(mut geo) = geo.as_mut() {
                    geo.loc.x += outer;
                    geo.loc.y += outer;
                    geo.size.w -= outer * 2;
                    geo.size.h -= outer * 2;

                    if tree.get(root).unwrap().data().geometry() == geo {
                        continue;
                    }
                }

                for node_id in tree
                    .traverse_pre_order_ids(root)
                    .unwrap()
                    .collect::<Vec<_>>()
                    .into_iter()
                {
                    let node = tree.get_mut(&node_id).unwrap();
                    let geo = stack.pop_front().unwrap_or(geo);
                    if let Some(geo) = geo {
                        let data = node.data_mut();
                        data.update_geometry(geo);
                        match data {
                            Data::Group {
                                orientation, sizes, ..
                            } => match orientation {
                                Orientation::Horizontal => {
                                    let mut previous = 0;
                                    for size in sizes {
                                        stack.push_back(Some(Rectangle::from_loc_and_size(
                                            (geo.loc.x, geo.loc.y + previous),
                                            (geo.size.w, *size),
                                        )));
                                        previous += *size;
                                    }
                                }
                                Orientation::Vertical => {
                                    let mut previous = 0;
                                    for size in sizes {
                                        stack.push_back(Some(Rectangle::from_loc_and_size(
                                            (geo.loc.x + previous, geo.loc.y),
                                            (*size, geo.size.h),
                                        )));
                                        previous += *size;
                                    }
                                }
                            },
                            Data::Mapped { mapped, .. } => {
                                if !mapped.is_fullscreen() {
                                    mapped.set_tiled(true);
                                    mapped.set_size(
                                        (geo.size.w - inner * 2, geo.size.h - inner * 2).into(),
                                    );
                                    mapped.configure();
                                }
                            }
                        }
                    } else if node.data().is_group() {
                        stack.push_back(None);
                        stack.push_back(None);
                    }
                }
            }
        }
    }

    pub fn mapped(&self) -> impl Iterator<Item = (&Output, &CosmicMapped, Point<i32, Logical>)> {
        self.trees
            .iter()
            .flat_map(|(output_data, tree)| {
                if let Some(root) = tree.root_node_id() {
                    Some(
                        tree.traverse_pre_order(root)
                            .unwrap()
                            .filter(|node| node.data().is_mapped(None))
                            .map(|node| match node.data() {
                                Data::Mapped {
                                    mapped,
                                    last_geometry,
                                    ..
                                } => (
                                    &output_data.output,
                                    mapped,
                                    output_data.location + last_geometry.loc,
                                ),
                                _ => unreachable!(),
                            }),
                    )
                } else {
                    None
                }
            })
            .flatten()
    }

    pub fn windows(&self) -> impl Iterator<Item = (Output, Window, Point<i32, Logical>)> + '_ {
        self.mapped().flat_map(|(output, mapped, loc)| {
            mapped
                .windows()
                .map(move |(w, p)| (output.clone(), w, p + loc))
        })
    }

    pub fn merge(&mut self, other: TilingLayout) {
        for (output_data, src) in other.trees {
            let mut dst = self.trees.entry(output_data.clone()).or_default();
            let orientation = match output_data.output.geometry().size {
                x if x.w >= x.h => Orientation::Horizontal,
                _ => Orientation::Vertical,
            };
            TilingLayout::merge_trees(src, &mut dst, orientation);
        }
        self.refresh();
    }

    fn merge_trees(src: Tree<Data>, dst: &mut Tree<Data>, orientation: Orientation) {
        if let Some(root_id) = src.root_node_id() {
            let mut stack = Vec::new();

            let root_node = src.get(root_id).unwrap();
            let new_node = Node::new(root_node.data().clone());
            let into_node_id = match dst.root_node_id().cloned() {
                Some(root) => TilingLayout::new_group(dst, &root, new_node, orientation),
                None => dst.insert(new_node, InsertBehavior::AsRoot),
            }
            .unwrap();

            stack.push((root_id.clone(), into_node_id));
            while let Some((src_id, dst_id)) = stack.pop() {
                for child_id in src.children_ids(&src_id).unwrap() {
                    let src_node = src.get(&child_id).unwrap();
                    let new_node = Node::new(src_node.data().clone());
                    let new_child_id = dst
                        .insert(new_node, InsertBehavior::UnderNode(&dst_id))
                        .unwrap();
                    stack.push((child_id.clone(), new_child_id));
                }
            }
        } else {
            *dst = src;
        }
    }

    pub fn render_output<R>(
        &self,
        output: &Output,
    ) -> Result<Vec<TilingRenderElement<R>>, OutputNotMapped>
    where
        R: Renderer + ImportAll,
        <R as Renderer>::TextureId: 'static,
    {
        let output_scale = output.current_scale().fractional_scale();
        let int_scale = output.current_scale().integer_scale();

        if !self.trees.contains_key(output) {
            return Err(OutputNotMapped);
        }

        Ok(self
            .mapped()
            .flat_map(|(o, mapped, loc)| {
                if o == output {
                    Some((mapped, loc))
                } else {
                    None
                }
            })
            .flat_map(|(mapped, loc)| {
                mapped.render_elements::<TilingRenderElement<R>>(
                    loc.to_physical(int_scale),
                    Scale::from(output_scale),
                )
            })
            .collect::<Vec<_>>())
    }
}

render_elements! {
    pub TilingRenderElement<R> where R: ImportAll;
    Window=CosmicMappedRenderElement<R>,
}
