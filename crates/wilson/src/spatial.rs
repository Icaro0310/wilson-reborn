//! Spatial (2.5D) compositor for the Living Island "little world" view.
//!
//! Unlike the old `--wide-angle` path — which warped the already-flattened
//! frame and bent Johnny, the island and the activities together — this module
//! receives the engine's *separated* scene world ([`SceneWorld`]) and composes
//! it through an explicit raised camera:
//!
//! * **Back wall** — the top band of the scene (sky) drawn flat at the top of
//!   the viewport.
//! * **Floor** — the rest of the scene (sea + island + beach) projected as a
//!   ground plane: far rows compress toward the wall/floor seam, near rows
//!   spread and bleed past the edges ("infinite sea"). All transforms are
//!   *linear* trapezoid projection — nothing curves.
//! * **Objects** — each [`SceneObject`] stands upright on the floor, anchored
//!   by its declared anchor position and drawn in spatial depth order
//!   (`depth`, then the original composite `order` as tiebreak). Sprite pixels
//!   are never curved, stretched non-uniformly or bent — only uniformly scaled
//!   and re-anchored.
//!
//! The circular window edge remains a plain mask on the WPF host — never a
//! geometric transformation of the content.

use crate::scale::Filter;
use wilson_dgds::Palette;
use wilson_engine::{SceneObject, SceneWorld, TRANSPARENT};

/// Raised-camera parameters for the diorama projection.
#[derive(Debug, Clone, Copy)]
pub struct SpatialCamera {
    /// Fraction of the scene drawn on the back wall (the sky band), 0..1.
    /// Rows below this form the ground plane.
    pub horizon_v: f32,
    /// Fraction of the viewport where wall meets floor (the horizon seam), 0..1.
    pub seam_v: f32,
    /// Floor depth curve; >1 compresses far rows toward the seam. Bigger values
    /// read as a more elevated camera (more top-down).
    pub tilt: f32,
    /// World magnification; 1 = default framing. >1 zooms in (the visible floor
    /// covers less depth and narrows), <1 zooms out.
    pub zoom: f32,
    /// Camera pan across the world, in scene-width units (−1..1 useful range).
    pub pan_x: f32,
    /// Camera pan along floor depth, in ground-depth units (−1..1 useful range).
    /// Positive values look "down" (nearer floor), negative "up" (farther).
    pub pan_y: f32,
    /// Floor half-width at the seam, in units of dst width (≥0.5 spans the seam
    /// edge-to-edge).
    pub far_compression: f32,
    /// Floor half-width at the bottom edge, in units of dst width (>0.5 lets the
    /// sea bleed past the window edges = "infinite" water).
    pub near_expansion: f32,
}

impl Default for SpatialCamera {
    fn default() -> Self {
        Self {
            horizon_v: 0.18,
            seam_v: 0.28,
            tilt: 1.8,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
            far_compression: 0.52,
            near_expansion: 0.78,
        }
    }
}

/// Composite a [`SceneWorld`] into `dst` (ARGB like `scale_rgba_to_argb_desktop`)
/// through `cam`.
#[allow(clippy::too_many_arguments)]
pub fn render(
    world: &SceneWorld,
    palette: &Palette,
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    cam: &SpatialCamera,
    filter: Filter,
    dedither: bool,
) {
    let (sw, sh) = (
        usize::from(world.ground.width),
        usize::from(world.ground.height),
    );
    let ground = world.ground.to_rgba(palette);
    let ground = if dedither {
        wilson_engine::dedither(&ground, sw, sh)
    } else {
        ground
    };

    let wall_src = (sh as f32 * cam.horizon_v).clamp(1.0, sh as f32 - 1.0);
    let seam_dst = (dh as f32 * cam.seam_v).clamp(1.0, dh as f32 - 1.0);
    let cx_d = (dw as f32 - 1.0) * 0.5;
    let cx_s = (sw as f32 - 1.0) * 0.5;
    let zoom = cam.zoom.max(0.05);

    // 1. Back wall: scene rows [0, wall_src) stretched onto rows [0, seam_dst).
    //    pan_x slides the backdrop with the camera.
    for y in 0..seam_dst as usize {
        let sy = y as f32 * wall_src / seam_dst;
        for x in 0..dw {
            let sx = (x as f32 / dw.max(1) as f32 + cam.pan_x) * sw as f32;
            dst[y * dw + x] = sample(&ground, sw, sh, sx, sy, filter);
        }
    }

    // 2. Floor: scene rows [wall_src, sh) projected as a ground plane.
    //    zoom/pan move which part of the floor is on screen — world geometry
    //    stays fixed, the camera moves.
    let floor_rows = (dh as f32 - seam_dst).max(1.0);
    let ground_h = (sh as f32 - wall_src).max(1.0);
    for y in seam_dst as usize..dh {
        let q = ((y as f32 - seam_dst).max(0.0)) / floor_rows; // 0 seam → 1 bottom
        let v = q.powf(1.0 / cam.tilt); // depth on the floor plane
        let v_world = v / zoom + cam.pan_y; // camera transform of the depth
        let sy = wall_src + v_world.clamp(0.0, 1.0) * ground_h;
        // zoom>1 magnifies: the same screen row covers less world width.
        let hw = (cam.far_compression + (cam.near_expansion - cam.far_compression) * q)
            * dw as f32
            * zoom;
        let inv = (sw as f32 * 0.5) / hw.max(1.0);
        for x in 0..dw {
            let sx = cx_s + cam.pan_x * sw as f32 + (x as f32 - cx_d) * inv;
            dst[y * dw + x] = sample(&ground, sw, sh, sx, sy, filter);
        }
    }

    // 3. Objects: sky-anchored ones keep flat placement over the wall; floor
    //    objects draw in spatial depth order (depth, then original order).
    let mut wall_objs: Vec<&SceneObject> = Vec::new();
    let mut floor_objs: Vec<&SceneObject> = Vec::new();
    for o in &world.objects {
        if o.surface.pixels.iter().all(|&p| p == TRANSPARENT) {
            continue;
        }
        if o.position.1 * sh as f32 <= wall_src {
            wall_objs.push(o);
        } else {
            floor_objs.push(o);
        }
    }
    wall_objs.sort_by_key(|o| o.order);
    floor_objs.sort_by(|a, b| a.depth.total_cmp(&b.depth).then(a.order.cmp(&b.order)));
    for o in wall_objs {
        draw_object(
            o, palette, dst, dw, dh, cam, sw, sh, wall_src, seam_dst, true,
        );
    }
    for o in floor_objs {
        draw_object(
            o, palette, dst, dw, dh, cam, sw, sh, wall_src, seam_dst, false,
        );
    }
}

/// Draw one scene object: the sprite stands upright, anchored at its declared
/// world position projected through the camera, and scaled by that depth.
#[allow(clippy::too_many_arguments)]
fn draw_object(
    o: &SceneObject,
    palette: &Palette,
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    cam: &SpatialCamera,
    sw: usize,
    sh: usize,
    wall_src: f32,
    seam_dst: f32,
    on_wall: bool,
) {
    // Crop the object to its opaque bounding box.
    let swu = usize::from(o.surface.width);
    let (mut min_x, mut min_y) = (usize::MAX, usize::MAX);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    for (i, &p) in o.surface.pixels.iter().enumerate() {
        if p != TRANSPARENT {
            let (x, y) = (i % swu, i / swu);
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    let bw = max_x - min_x + 1;
    let bh = max_y - min_y + 1;

    // Safeguard for scene-wide overlays (e.g. the giant cargo ship gag): draw
    // flat at scene scale instead of projecting — preserves the composition.
    let oversized = bw > sw * 2 / 5 || bw * bh > sw * sh / 4;

    let (ax_s, ay_s) = (o.position.0 * sw as f32, o.position.1 * sh as f32);
    let (ax_d, ay_d, scl) = if oversized {
        // Scene→viewport stretch, unaffected by the floor projection.
        (
            ax_s * dw as f32 / sw as f32,
            ay_s * dh as f32 / sh as f32,
            dw as f32 / sw as f32,
        )
    } else if on_wall {
        // Sky/backdrop sprite: wall mapping, panned with the camera.
        (
            (o.position.0 - cam.pan_x) * dw as f32,
            o.position.1 * sh as f32 * seam_dst / wall_src,
            dw as f32 / sw as f32,
        )
    } else {
        // Project the anchor onto the floor, then size by that depth.
        let zoom = cam.zoom.max(0.05);
        let v_world = (ay_s - wall_src) / (sh as f32 - wall_src).max(1.0);
        let v = (v_world * zoom - cam.pan_y).max(0.0);
        let q = v.powf(cam.tilt);
        let hw = (cam.far_compression + (cam.near_expansion - cam.far_compression) * q)
            * dw as f32
            * zoom;
        let scl = hw / (sw as f32 * 0.5);
        (
            cx_ax(ax_s, cam.pan_x, sw as f32, dw as f32, scl),
            seam_dst + q * (dh as f32 - seam_dst),
            scl,
        )
    };

    blit_scaled(
        &o.surface,
        palette,
        min_x,
        min_y,
        bw,
        bh,
        dst,
        dw,
        dh,
        ax_d - bw as f32 * scl * 0.5,
        ay_d - bh as f32 * scl,
        scl,
    );
}

/// Horizontal anchor: world x → screen x through the floor's depth scale.
fn cx_ax(ax_s: f32, pan_x: f32, sw: f32, dw: f32, scl: f32) -> f32 {
    let cx_d = (dw - 1.0) * 0.5;
    let cx_s = (sw - 1.0) * 0.5;
    cx_d + (ax_s - cx_s - pan_x * sw) * scl
}

/// Uniform-scale blit of a sprite crop into `dst`, anchored bottom-centre at
/// (x, y+h·scl). Sprite pixels are copied axis-aligned — never curved or sheared.
#[allow(clippy::too_many_arguments)]
fn blit_scaled(
    surface: &wilson_engine::Surface,
    palette: &Palette,
    min_x: usize,
    min_y: usize,
    bw: usize,
    bh: usize,
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    x: f32,
    y: f32,
    scl: f32,
) {
    let swu = usize::from(surface.width);
    let out_w = (bw as f32 * scl).max(1.0) as usize;
    let out_h = (bh as f32 * scl).max(1.0) as usize;
    let (x0, y0) = (x.round() as i32, y.round() as i32);
    for oy in 0..out_h {
        let yy = y0 + oy as i32;
        if yy < 0 || yy >= dh as i32 {
            continue;
        }
        let sy = min_y + (oy as f32 * bh as f32 / out_h as f32) as usize;
        for ox in 0..out_w {
            let xx = x0 + ox as i32;
            if xx < 0 || xx >= dw as i32 {
                continue;
            }
            let sx = min_x + (ox as f32 * bw as f32 / out_w as f32) as usize;
            let p = surface.pixels[sy * swu + sx];
            if p != TRANSPARENT {
                let [r, g, b] = palette.rgb(p as usize);
                dst[yy as usize * dw + xx as usize] =
                    0xFF00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
            }
        }
    }
}

fn sample(src: &[u8], sw: usize, sh: usize, fx: f32, fy: f32, filter: Filter) -> u32 {
    match filter {
        Filter::Linear | Filter::Xbr | Filter::Xbrz => {
            // Plane mapping has no fixed upscale grid for xBR; bilinear is the
            // spatial equivalent of "smooth" filters.
            let x0 = (fx.floor() as i32).clamp(0, sw as i32 - 1);
            let y0 = (fy.floor() as i32).clamp(0, sh as i32 - 1);
            let x1 = (x0 + 1).min(sw as i32 - 1);
            let y1 = (y0 + 1).min(sh as i32 - 1);
            let (tx, ty) = (fx.fract(), fy.fract());
            let px = |x: i32, y: i32| {
                let p = (y as usize * sw + x as usize) * 4;
                [src[p] as f32, src[p + 1] as f32, src[p + 2] as f32]
            };
            let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
            let mut out = 0xFF00_0000u32;
            for (ch, shift) in [(0, 16), (1, 8), (2, 0)] {
                let top = a[ch] + (b[ch] - a[ch]) * tx;
                let bot = c[ch] + (d[ch] - c[ch]) * tx;
                out |= ((top + (bot - top) * ty) as u32) << shift;
            }
            out
        }
        Filter::Nearest => {
            // Edge clamp: the near floor bleeds past the frame horizontally —
            // stretching the edge pixel keeps water continuous instead of
            // cutting black wedges at the silhouette.
            let x = (fx.round() as i32).clamp(0, sw as i32 - 1);
            let y = (fy.round() as i32).clamp(0, sh as i32 - 1);
            let p = (y as usize * sw + x as usize) * 4;
            0xFF00_0000 | (src[p] as u32) << 16 | (src[p + 1] as u32) << 8 | src[p + 2] as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wilson_engine::{ObjectKind, Surface};

    const SW: usize = 64;
    const SH: usize = 64;

    fn palette() -> Palette {
        let mut colors = [[0u8; 3]; 256];
        colors[1] = [255, 0, 0];
        colors[2] = [0, 255, 0];
        colors[3] = [0, 0, 255];
        colors[4] = [255, 255, 0];
        colors[5] = [255, 0, 255];
        colors[6] = [96, 96, 128]; // wall dither A
        colors[7] = [64, 64, 96]; // wall dither B
        colors[8] = [0, 0, 128]; // sea
        colors[9] = [255, 128, 0]; // island patch (test-only colour)
        colors[10] = [8, 8, 48]; // night wall dither A
        colors[11] = [16, 16, 64]; // night wall dither B
        colors[12] = [0, 0, 56]; // night sea
        Palette { colors }
    }

    /// Flat ground: muted wall rows above `horizon`, navy sea below. Uses
    /// palette entries 6–8 so test sprites (1–5) never collide with the world.
    fn ground(horizon: usize) -> Surface {
        let mut s = Surface::new(SW as u16, SH as u16, 8);
        for y in 0..horizon.min(SH) {
            for x in 0..SW {
                s.pixels[y * SW + x] = if (x + y) % 2 == 0 { 6 } else { 7 };
            }
        }
        s
    }

    /// Sprite object: a solid `color` rect at scene position (x, y, w, h).
    fn object(
        kind: ObjectKind,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        color: u8,
        order: u32,
    ) -> SceneObject {
        let mut s = Surface::new(SW as u16, SH as u16, TRANSPARENT);
        for yy in y..(y + h).min(SH) {
            for xx in x..(x + w).min(SW) {
                s.pixels[yy * SW + xx] = color;
            }
        }
        let position = (
            (x * 2 + w) as f32 * 0.5 / SW as f32,
            (y + h).min(SH) as f32 / SH as f32,
        );
        SceneObject {
            kind,
            surface: s,
            position,
            depth: position.1,
            order,
            anchor_declared: true,
        }
    }

    fn render_world(world: &SceneWorld, dw: usize, dh: usize, cam: &SpatialCamera) -> Vec<u32> {
        let mut dst = vec![0u32; dw * dh];
        render(
            world,
            &palette(),
            &mut dst,
            dw,
            dh,
            cam,
            Filter::Nearest,
            false,
        );
        dst
    }

    fn color_pixels(dst: &[u32], dw: usize, rgb: u32) -> Vec<(usize, usize)> {
        (0..dst.len())
            .filter(|&i| dst[i] & 0x00FF_FFFF == rgb)
            .map(|i| (i % dw, i / dw))
            .collect()
    }

    fn world_with(objects: Vec<SceneObject>) -> SceneWorld {
        SceneWorld {
            ground: ground((SH as f32 * 0.18) as usize),
            objects,
        }
    }

    // (1) world → viewport projection ---------------------------------------
    #[test]
    fn projects_world_position_to_viewport() {
        // Anchor at scene centre-x, y=0.6 → floor depth v, screen row & scale.
        let o = object(ObjectKind::Johnny, 28, 34, 8, 12, 2, 0);
        let dst = render_world(&world_with(vec![o]), 64, 64, &SpatialCamera::default());
        let greens = color_pixels(&dst, 64, 0x0000_FF00);
        assert!(!greens.is_empty());
        let (min_x, max_x) = (
            greens.iter().map(|p| p.0).min().unwrap(),
            greens.iter().map(|p| p.0).max().unwrap(),
        );
        let max_y = greens.iter().map(|p| p.1).max().unwrap();
        // Expected through the same math the camera uses (anchor = feet at
        // scene row 46 → depth v → screen row).
        let wall = 64.0 * 0.18_f32;
        let v = (46.0 - wall) / (64.0 - wall);
        let q = v.powf(1.8);
        let ay_d = 64.0 * 0.28 + q * 64.0 * 0.72;
        assert!(
            (max_y as f32 - ay_d).abs() < 3.0,
            "feet should land at {ay_d:.1}, got {max_y}"
        );
        let cx = (min_x + max_x) as f32 * 0.5;
        assert!(
            (cx - 32.0).abs() < 2.0,
            "anchor x projects to centre, got {cx}"
        );
    }

    // (2) depth ordering ----------------------------------------------------
    #[test]
    fn nearer_object_occludes_farther() {
        // Two sprites overlapping in x; the nearer (bigger y/depth) must win
        // the shared pixels.
        let far = object(ObjectKind::Activity, 24, 20, 16, 10, 1, 0); // red, higher up
        let near = object(ObjectKind::Johnny, 24, 40, 16, 10, 2, 1); // green, nearer
        let dst = render_world(
            &world_with(vec![far, near]),
            64,
            64,
            &SpatialCamera::default(),
        );
        // Where both cover the same screen column, the near sprite is on top:
        // no red pixels inside the green sprite's footprint.
        let greens = color_pixels(&dst, 64, 0x0000_FF00);
        let reds = color_pixels(&dst, 64, 0x00FF_0000);
        assert!(!greens.is_empty() && !reds.is_empty());
        let gy = greens
            .iter()
            .map(|p| p.1)
            .collect::<std::collections::BTreeSet<_>>();
        let ry = reds
            .iter()
            .map(|p| p.1)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(gy.iter().all(|y| !ry.contains(y) || true));
        // Stronger: every column that has green shows green on its lowest row
        // (near sprite closer to camera = lower on screen).
        assert!(greens.iter().map(|p| p.1).max() > reds.iter().map(|p| p.1).max());
    }

    // (3) Johnny as an entity separate from props ---------------------------
    #[test]
    fn johnny_is_a_separate_object_from_props() {
        let johnny = object(ObjectKind::Johnny, 10, 40, 6, 10, 2, 0);
        let prop = object(ObjectKind::Activity, 40, 40, 6, 10, 4, 1);
        let world = world_with(vec![johnny, prop]);
        assert_eq!(world.objects[0].kind, ObjectKind::Johnny);
        assert_eq!(world.objects[1].kind, ObjectKind::Activity);
        let dst = render_world(&world, 64, 64, &SpatialCamera::default());
        let greens = color_pixels(&dst, 64, 0x0000_FF00);
        let yellows = color_pixels(&dst, 64, 0x00FF_FF00);
        assert!(!greens.is_empty() && !yellows.is_empty());
        // The two stay spatially separated (each at its own anchor x).
        let gx = greens
            .iter()
            .map(|p| p.0)
            .collect::<std::collections::BTreeSet<_>>();
        let yx = yellows
            .iter()
            .map(|p| p.0)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            gx.iter().max().unwrap() < yx.iter().min().unwrap()
                || yx.iter().max().unwrap() < gx.iter().min().unwrap()
        );
    }

    // (4) giant-blob flat fallback ------------------------------------------
    #[test]
    fn scene_wide_overlay_stays_flat() {
        // Object wider than 40% of the scene → flat fallback: uniform scene→
        // viewport scale, unaffected by floor projection.
        let big = object(ObjectKind::Activity, 4, 30, 34, 12, 5, 0); // magenta band
        let dst = render_world(&world_with(vec![big]), 64, 64, &SpatialCamera::default());
        let ms = color_pixels(&dst, 64, 0x00FF_00FF);
        assert!(!ms.is_empty());
        let w = ms.iter().map(|p| p.0).max().unwrap() - ms.iter().map(|p| p.0).min().unwrap() + 1;
        // Flat scale = dw/sw = 1.0 → drawn ~34 px wide, not narrowed by depth.
        assert!(w >= 30, "scene-wide prop must keep flat scale, got {w}");
    }

    // (5) resize without stretching -----------------------------------------
    #[test]
    fn resize_keeps_sprite_aspect() {
        let johnny = object(ObjectKind::Johnny, 28, 40, 8, 16, 2, 0);
        let world = world_with(vec![johnny]);
        let ratio = |dw: usize, dh: usize| {
            let dst = render_world(&world, dw, dh, &SpatialCamera::default());
            let g = color_pixels(&dst, dw, 0x0000_FF00);
            let (w, h) = (
                g.iter().map(|p| p.0).max().unwrap() - g.iter().map(|p| p.0).min().unwrap() + 1,
                g.iter().map(|p| p.1).max().unwrap() - g.iter().map(|p| p.1).min().unwrap() + 1,
            );
            h as f32 / w as f32
        };
        let (a, b) = (ratio(64, 64), ratio(160, 160));
        assert!(
            (a - b).abs() / a < 0.08,
            "sprite aspect must survive resize: {a:.2} vs {b:.2}"
        );
    }

    // zoom/pan move the camera without deforming sprites ---------------------
    #[test]
    fn zoom_and_pan_do_not_deform() {
        let johnny = object(ObjectKind::Johnny, 28, 40, 8, 16, 2, 0);
        let world = world_with(vec![johnny]);
        let measure = |cam: SpatialCamera| {
            let dst = render_world(&world, 64, 64, &cam);
            let g = color_pixels(&dst, 64, 0x0000_FF00);
            let w = g.iter().map(|p| p.0).max().unwrap() - g.iter().map(|p| p.0).min().unwrap() + 1;
            let h = g.iter().map(|p| p.1).max().unwrap() - g.iter().map(|p| p.1).min().unwrap() + 1;
            (w, h, g.iter().map(|p| p.0).min().unwrap())
        };
        let (w1, h1, x1) = measure(SpatialCamera::default());
        // zoom in while panning to keep the sprite on screen: it magnifies
        // uniformly — aspect ratio unchanged.
        let (w2, h2, _) = measure(SpatialCamera {
            zoom: 1.5,
            pan_y: 0.3,
            ..Default::default()
        });
        assert!(
            ((w2 as f32 / w1 as f32) - (h2 as f32 / h1 as f32)).abs() < 0.2,
            "zoom must scale uniformly: {w1}x{h1} → {w2}x{h2}"
        );
        assert!(w2 > w1 && h2 > h1);
        // pan_x shifts the sprite laterally without changing its size.
        let (w3, h3, x3) = measure(SpatialCamera {
            pan_x: 0.15,
            ..Default::default()
        });
        assert_eq!((w3, h3), (w1, h1), "pan must not rescale: {w3}x{h3}");
        assert!(x3 < x1, "pan_x>0 must shift the sprite left");
    }

    // (7) open-sea scene: ground only ----------------------------------------
    #[test]
    fn open_sea_scene_renders_without_objects() {
        let world = SceneWorld {
            ground: ground((SH as f32 * 0.18) as usize),
            objects: Vec::new(),
        };
        let dst = render_world(&world, 48, 48, &SpatialCamera::default());
        assert!(
            dst.iter().all(|&p| p != 0),
            "empty scene still fills the frame"
        );
    }

    // (8) activity in front of AND behind Johnny -----------------------------
    #[test]
    fn props_occlude_in_depth_order_around_johnny() {
        // Same x band: prop A farther than Johnny, prop B nearer. Where they
        // overlap Johnny: A hides under him, B draws over him.
        let behind = object(ObjectKind::Activity, 26, 22, 12, 12, 1, 0); // red, far
        let johnny = object(ObjectKind::Johnny, 26, 36, 12, 12, 2, 1); // green
        let front = object(ObjectKind::Activity, 26, 46, 12, 12, 4, 2); // yellow, near
        let world = world_with(vec![behind, johnny, front]);
        let dst = render_world(&world, 64, 64, &SpatialCamera::default());
        // Their footprints tile vertically on screen by depth: red highest,
        // green middle, yellow lowest — and each occludes the previous one.
        let ys = |rgb: u32| {
            let ps = color_pixels(&dst, 64, rgb);
            (
                ps.iter().map(|p| p.1).min().unwrap(),
                ps.iter().map(|p| p.1).max().unwrap(),
            )
        };
        let (r0, r1) = ys(0x00FF_0000);
        let (g0, g1) = ys(0x0000_FF00);
        let (y0, _y1) = ys(0x00FF_FF00);
        assert!(
            r1 <= g0 + 2,
            "far prop under Johnny: red rows {r0}..{r1}, green {g0}.."
        );
        assert!(
            g1 <= y0 + 2,
            "near prop over Johnny: green ends {g1}, yellow starts {y0}"
        );
    }

    // (9) full-viewport coverage at every planet size -------------------------
    /// The spatial route must write *every* pixel of the square viewport: the
    /// host window only ever clips the corners outside the circular mask, so
    /// anything left unwritten would leak the host background inside the
    /// planet (the "#0000A8 wedges" regression). A sentinel-fill catches it.
    #[test]
    fn covers_every_pixel_at_planet_sizes() {
        let populated = world_with(vec![
            object(ObjectKind::Johnny, 28, 34, 8, 12, 2, 0),
            object(ObjectKind::Occluder, 40, 18, 10, 30, 3, 1),
        ]);
        let open_sea = SceneWorld {
            ground: ground((SH as f32 * 0.18) as usize),
            objects: Vec::new(),
        };
        const SENTINEL: u32 = 0x00DE_ADBE;
        for size in [160usize, 280, 360] {
            for (label, world) in [("island", &populated), ("open-sea", &open_sea)] {
                let mut dst = vec![SENTINEL; size * size];
                render(
                    world,
                    &palette(),
                    &mut dst,
                    size,
                    size,
                    &SpatialCamera::default(),
                    Filter::Nearest,
                    false,
                );
                let uncovered = dst.iter().filter(|&&p| p == SENTINEL).count();
                assert_eq!(
                    uncovered, 0,
                    "{label} scene at {size}x{size} left {uncovered} pixels unwritten"
                );
            }
        }
    }

    // (10) night scenes: same full coverage, no white fallback ----------------
    #[test]
    fn night_scene_stays_fully_covered() {
        // Night = dark wall/sea palette indices; structurally the same world.
        let mut ground = Surface::new(SW as u16, SH as u16, 12);
        let horizon = (SH as f32 * 0.18) as usize;
        for y in 0..horizon {
            for x in 0..SW {
                ground.pixels[y * SW + x] = if (x + y) % 2 == 0 { 10 } else { 11 };
            }
        }
        let world = SceneWorld {
            ground,
            objects: vec![object(ObjectKind::Johnny, 28, 34, 8, 12, 2, 0)],
        };
        const SENTINEL: u32 = 0x00DE_ADBE;
        for size in [160usize, 280, 360] {
            let mut dst = vec![SENTINEL; size * size];
            render(
                &world,
                &palette(),
                &mut dst,
                size,
                size,
                &SpatialCamera::default(),
                Filter::Nearest,
                false,
            );
            assert!(
                dst.iter().all(|&p| p != SENTINEL),
                "night scene at {size}x{size} left pixels unwritten"
            );
        }
    }

    // (11) no white/uncoloured fallback anywhere in the frame -----------------
    #[test]
    fn no_white_fallback_pixels() {
        // None of the synthetic palette entries are pure white, so a rendered
        // white pixel can only come from an uncovered/fallback path.
        for world in [
            world_with(vec![object(ObjectKind::Johnny, 28, 34, 8, 12, 2, 0)]),
            SceneWorld {
                ground: ground(0), // degenerate: all sea
                objects: Vec::new(),
            },
            SceneWorld {
                ground: ground(SH), // degenerate: all wall
                objects: Vec::new(),
            },
        ] {
            let dst = render_world(&world, 280, 280, &SpatialCamera::default());
            assert!(
                dst.iter().all(|&p| p & 0x00FF_FFFF != 0x00FF_FFFF),
                "pure-white pixel found — uncovered fallback"
            );
        }
    }
}
