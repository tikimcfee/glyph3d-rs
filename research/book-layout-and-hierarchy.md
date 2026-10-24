# Book Layout Style, Stacking Primitives & Spatial Hierarchy

This document details the architecture, mathematical formulations, and rendering primitives for the **Book Layout Style**, **Rolodex/Splay Stacking**, **Circuit-Routed Connectivity Lines**, and **Hierarchical Labels**, adapted from the `glyph3d` spatial engine into native Rust / `wgpu` / `bevy_ecs`.

---

## 1. The Core Philosophy: Volume Compression

### The Problem with 2D Planar Layouts
On a large codebase ($10\text{k}–75\text{k}$ files like `torvalds/linux`), arranging files along an expansive 2D plane (whether grid, shelf, or treemap) creates an uncontrollable spatial footprint:
- The camera must fly kilometers back to see module boundaries.
- At distance, files become subpixel noise, losing readability.
- Spatial locality is broken because siblings push adjacent packages far apart.

### The Book Paradigm
A **Book** is a durable 3D spatial carrier that binds content into sequential pages:
- A directory or package becomes a **Library Volume**.
- Files inside the directory become **Sheets** (pages) stacked within the volume.
- Footprint is compressed from $O(N)$ planar area to $O(1)$ surface footprint with $Z$-depth stacking.

```
            [ Flat Planar: O(N) Area ]
   ┌───┐  ┌───┐  ┌───┐  ┌───┐  ┌───┐  ┌───┐
   │ 1 │  │ 2 │  │ 3 │  │ 4 │  │ 5 │  │ 6 │  ... (sprawls infinitely)
   └───┘  └───┘  └───┘  └───┘  └───┘  └───┘

            [ Book Volume: O(1) Footprint ]
              ┌──────┐
             /│ 3    │
            / │ 2    │ (Z-depth rolodex stacking)
           ┌──┴───┐  │
           │ 1    │ /
           │ Head │/
           └──────┘
```

---

## 2. Stacking Primitives & Presentation Forms

Every multi-sheet Book operates under a **Slot Law**—a mathematical mapping from sheet index $i$ to local 3D transform $(\vec{p}_i, \mathbf{q}_i)$:

### A. Deck Form (The Rolodex)
In `Deck` mode, the current sheet (`head`) fronts the volume at $Z = 0$, while inactive sheets recede in depth:

$$\Delta i = (i - \text{head}) \pmod N$$
$$\vec{p}_i = \left( 0,\; 0,\; -\Delta i \cdot z_{\text{pitch}} \right)$$

- **$z_{\text{pitch}}$**: Spacing between sheets in depth (e.g. $90$ world units).
- **Recency / Page Ordering**:
  - `order = -1` (Recency / Agent): Newer turns front; older turns recede into depth.
  - `order = +1` (Sequential / Library): Page 1 fronts; pages 2, 3, ... recede in reading order.
- **Turning**: Advancing the head wraps past sheets to the back of the deck.

### B. Splay Form (The Open Grid)
In `Splay` mode, the book is laid open as an $M \times N$ planar grid, exposing all pages simultaneously for quick overview:

$$\text{col} = i \pmod{\text{cols}}, \quad \text{row} = \lfloor i / \text{cols} \rfloor$$
$$X_i = \text{col} \cdot (W_{\text{page}} + \text{gap}_x)$$
$$Y_i = -\text{row} \cdot (H_{\text{page}} + \text{gap}_y)$$
$$Z_i = \begin{cases} +z_{\text{lift}} & \text{if } i = \text{head} \text{ (bookmark)} \\ 0 & \text{otherwise} \end{cases}$$

- **The Floating Bookmark**: The active sheet (`head`) floats $z_{\text{lift}}$ forward ($+Z$, e.g. $30$ units), serving as a visible physical bookmark across the grid.
- **Paging**: Paging in splay mode smoothly glides the bookmark across the grid rather than moving the sheets.

### C. Fitted Page Rects & Spreads
- **Fitted Form**: Content is contain-fitted into a standardized `pageW × pageH` rect with an aspect-preserving scale factor:
  $$s = \min\left(\frac{W_{\text{page}}}{W_{\text{content}}}, \frac{H_{\text{page}}}{H_{\text{content}}}\right)$$
- **Spreads**: A sheet can hold a **Verso** (left page) and **Recto** (right page) centered around a central gutter.

---

## 3. Motion & Animation Primitives

Relayout must be **something you watch happen**, not an instantaneous teleport.

### Frame-Rate Independent Exponential Easing
Nodes glide from current position $\vec{p}$ toward target $\vec{p}_{\text{target}}$ using the exponential decay formula:

$$k = 1.0 - e^{-\text{rate} \cdot \Delta t}$$
$$\vec{p} \mathrel{+}= (\vec{p}_{\text{target}} - \vec{p}) \cdot k$$
$$\mathbf{q} = \text{slerp}(\mathbf{q}, \mathbf{q}_{\text{target}}, k)$$

- **$\text{rate} = 7–9\,\text{s}^{-1}$**: Reaches $\sim 98\%$ of destination in $\sim 300\text{ ms}$.
- **Rotational Slerp**: Preserves continuous facing angles when books pivot into carrel arcs or shelves.
- **Event-Driven Snapshots**:
  1. `onBeforeRelayout`: Snapshots live transforms of all persistent nodes.
  2. `onRelayout`: Layout engine stamps new targets.
  3. `first update()`: Rewinds nodes to snapshot positions and animates toward targets. If a relayout occurs mid-flight, the live in-flight position becomes the new starting point, eliminating visual snapping.

---

## 4. Connectivity Lines: Circuit-Routed Ownership Traces

Connecting folders to files using straight diagonals produces visual clutter resembling tangled string that crosses content faces.

### The PCB Bus Routing Invariant
All ownership traces are routed strictly within **inter-row gutters** and **outer margins**, ensuring lines never cross code or text.

```
       [ Directory Hub ]
              │
    ┌─────────┴───────────────────────────┐  <-- Top Trunk Bus
    │
    │  (Outer Bus Margin)
    ├───┬─────────────────────────────────┐  <-- Rail in row gutter
    │   │                                 │
    │   ▼ Drop to Pin                     ▼ Drop to Pin
    │  ┌──────────┐                      ┌──────────┐
    │  │ Book 1   │                      │ Book 2   │
    │  └──────────┘                      └──────────┘
```

1. **Pin**: The connection point on each child, placed at the **top-center** of its bounding frame:
   $$\vec{p}_{\text{pin}} = \left( X_{\text{center}},\; Y_{\text{top}},\; Z_{\text{front}} \right)$$
2. **Hub**: The directory's origin coordinate.
3. **Trunk Bus**: Exits the hub and runs down the outside left margin (`busMargin`, e.g. $74$ units outside all children).
4. **Rails**: Horizontal runs branching off the trunk bus into the inter-row gutters (`railGap`, e.g. $100$ units above child tops).
5. **Drops**: Short vertical segments dropping directly from the rail onto the child's pin.
6. **Chamfered 45° Corners**: Rounded/chamfered turns (`chamfer = 20.5`) reinforce the aesthetic of circuit traces.

### Stroke Weight & Hierarchical Depth Decay
Traces have physical world-space thickness that diminishes with tree depth:

$$\text{weight} = \max\left(\text{weight}_{\text{min}},\; \text{weight}_{\text{base}} \cdot (\text{decay})^{\text{depth}}\right)$$

- $\text{weight}_{\text{base}} = 10$, $\text{decay} = 0.95$, $\text{weight}_{\text{min}} = 0.3$.
- Top-level directories feature strong, authoritative trunks; deep subpackages feature fine hairline leads.
- **Semantic Colors**:
  - File traces: `colorA` (`#4a8acc` Blue).
  - Directory traces & Trunk Bus: `colorB` (`#cc7a4a` Warm Orange).

---

## 5. Hierarchical Labels & Physical Proximity LOD

Containers must self-identify without producing text clutter.

### Proportional Container Fit
Labels are sized proportional to their container's width:
$$\text{scale} = \text{clamp}\left(\frac{W_{\text{container}} \cdot \text{fit}}{\text{text\_len} \cdot \text{glyph\_w}},\; \text{scale}_{\text{min}},\; \text{scale}_{\text{max}}\right)$$
- Broad root packages receive large titles readable from high altitude.
- Deep, compact packages reveal legible titles as the camera approaches.

### Approach Spectrum (Proximity Easing)
As the camera moves from distance $\text{fadeStart}$ ($320$) to $\text{fadeEnd}$ ($110$):
- Label scale eases from container scale down to name-tag scale (`nearScale: 7.3`).
- When arrived, a solid dark backplate (`plate: 1`, `#0c0f16` at $62\%$ opacity) appears behind the text, ensuring contrast against whatever code sits behind it.

### Hierarchical Hover Swell
Hovering any file or folder triggers a live `hoverBoost` ($1.4\times$) on its label and its entire ancestor directory chain, highlighting the complete containment path up to the repository root.

---

## 6. Implementation Map for `glyph3d-rs`

In the Rust renderer (`native/src/`):

1. **ECS Hierarchy (`bevy_ecs` + `bevy_transform`)**:
   - Entities hold `Parent` and `Children` components.
   - Volumes and Books are parent nodes; sheets and mounts are child entities whose local transforms update via `TransformPropagate`.
2. **Book Component & System**:
   - `Book` component stores `head`, `count`, `form: BookForm::{Deck, Splay}`, and animation state.
   - A `book_layout_system` computes target sheet transforms and applies exponential easing.
3. **Trace Mesh Generator (`native/src/glyph_scene/mesh.rs`)**:
   - Generates instanced 3D chamfered line quads from directory bounding boxes.
   - Renders with `WireLineMaterial` in `glyph_scene/render.rs`.
4. **Labels**:
   - Uses the existing `GlyphField` instanced text pipeline, rendering label glyphs and backplates in a dedicated group.
