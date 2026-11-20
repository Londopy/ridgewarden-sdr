//! A small map drawn as inline SVG: receivers over the mesh's known node positions. No
//! tiles, so it works with the internet down; positions are plotted on a flat projection
//! scaled for latitude, which is plenty for a county-sized mesh.

use std::fmt::Write as _;

use ridgewarden_client::util::esc;

pub const CSS: &str = "<style>svg.map{width:100%;height:auto;max-height:400px;display:block;background:var(--card);\
border-radius:6px;margin:8px 0}.map .ctx{fill:var(--mute);opacity:.5}.map circle.rx{stroke:var(--bg);stroke-width:1.5}\
.map .green{fill:var(--green)}.map .yellow{fill:var(--yellow)}.map .red{fill:var(--red)}\
.map .unknown{fill:var(--unknown)}.map text{fill:var(--fg);font-size:14px}\
.map circle.spot{fill:none;stroke:var(--fg);stroke-width:2.5}\
.map path.ellipse{fill:var(--red);fill-opacity:.15;stroke:var(--red);stroke-width:1.5}\
.map path.ray{fill:none;stroke:var(--yellow);stroke-width:1.5;stroke-dasharray:6 4}</style>";

/// A line or outline drawn under the points: a bearing or an error ellipse.
pub struct Shape {
    /// (lat, lon)
    pub pts: Vec<(f64, f64)>,
    /// "ray" or "ellipse"
    pub class: &'static str,
    pub tip: String,
    /// whether the map's frame should take it in; a bearing runs off the edge instead
    pub fit: bool,
}

pub struct Pt {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
    pub tip: String,
    /// green, yellow, red, unknown, or spot for a surveyed position
    pub class: &'static str,
    pub r: f64,
}

const W: f64 = 640.0;
const PAD: f64 = 28.0;

/// The map, or None when nothing has a position.
pub fn render(
    points: &[Pt],
    context: &[(String, f64, f64)],
    shapes: &[Shape],
    title: &str,
) -> Option<String> {
    if points.is_empty() {
        return None;
    }
    let all = points
        .iter()
        .map(|p| (p.lat, p.lon))
        .chain(context.iter().map(|c| (c.1, c.2)))
        .chain(
            shapes
                .iter()
                .filter(|s| s.fit)
                .flat_map(|s| s.pts.iter().copied()),
        );
    let (mut s, mut n, mut w, mut e) = (90f64, -90f64, 180f64, -180f64);
    for (la, lo) in all {
        s = s.min(la);
        n = n.max(la);
        w = w.min(lo);
        e = e.max(lo);
    }
    let kx = ((s + n) / 2.0).to_radians().cos().max(0.05);
    // at least ~5 km across, so a single site isn't blown up to fill the frame
    let span_x = ((e - w) * kx).max(0.05);
    let span_y = (n - s).max(0.05);
    let (cx, cy) = ((w + e) / 2.0 * kx, (s + n) / 2.0);
    let scale = ((W - 2.0 * PAD) / span_x).min((420.0 - 2.0 * PAD) / span_y);
    let h = (span_y * scale + 2.0 * PAD).max(160.0);
    let x = |lon: f64| W / 2.0 + (lon * kx - cx) * scale;
    let y = |lat: f64| h / 2.0 - (lat - cy) * scale;

    let mut o = format!(
        "<svg class=map viewBox=\"0 0 {W} {h:.0}\" role=img aria-label=\"{t}\"><title>{t}</title>",
        t = esc(title)
    );
    for (name, la, lo) in context {
        let _ = write!(
            o,
            "<circle class=ctx cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\"><title>{}</title></circle>",
            x(*lo),
            y(*la),
            esc(name)
        );
    }
    for sh in shapes {
        let mut d = String::new();
        for (i, (la, lo)) in sh.pts.iter().enumerate() {
            let _ = write!(
                d,
                "{}{:.1},{:.1}",
                if i == 0 { "M" } else { "L" },
                x(*lo),
                y(*la)
            );
        }
        if sh.class == "ellipse" {
            d.push('Z');
        }
        let _ = write!(
            o,
            "<path class={} d=\"{d}\"><title>{}</title></path>",
            sh.class,
            esc(&sh.tip)
        );
    }
    for p in points {
        let (px, py) = (x(p.lon), y(p.lat));
        let _ = write!(
            o,
            "<circle class=\"rx {}\" cx=\"{px:.1}\" cy=\"{py:.1}\" r=\"{:.1}\"><title>{}</title></circle>",
            p.class,
            p.r,
            esc(&p.tip)
        );
        // labels near the right edge go on the left of their dot
        let (lx, anchor) = if px > W * 0.7 {
            (px - p.r - 4.0, "end")
        } else {
            (px + p.r + 4.0, "start")
        };
        let _ = write!(
            o,
            "<text x=\"{lx:.1}\" y=\"{:.1}\" text-anchor={anchor}>{}</text>",
            py + 4.0,
            esc(&p.label)
        );
    }
    o.push_str("</svg>");
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(lat: f64, lon: f64, label: &str) -> Pt {
        Pt {
            lat,
            lon,
            label: label.into(),
            tip: format!("{label} tip"),
            class: "green",
            r: 6.0,
        }
    }

    fn circles(svg: &str) -> Vec<(f64, f64)> {
        svg.split("<circle")
            .skip(1)
            .map(|c| {
                let num = |k: &str| -> f64 {
                    c.split(&format!("{k}=\""))
                        .nth(1)
                        .unwrap()
                        .split('"')
                        .next()
                        .unwrap()
                        .parse()
                        .unwrap()
                };
                (num("cx"), num("cy"))
            })
            .collect()
    }

    #[test]
    fn north_is_up_east_is_right_and_labels_are_escaped() {
        let svg = render(
            &[pt(35.63, -120.69, "<Paso>"), pt(35.18, -120.55, "Lopez")],
            &[("tst-cuesta".into(), 35.33, -120.62)],
            &[],
            "Receivers",
        )
        .unwrap();
        let c = circles(&svg);
        let (ctx, paso, lopez) = (c[0], c[1], c[2]);
        assert!(paso.1 < lopez.1, "Paso is further north, so higher up");
        assert!(lopez.0 > paso.0, "Lopez is further east");
        assert!(ctx.1 > paso.1 && ctx.1 < lopez.1);
        assert!(c.iter().all(|(x, _)| (0.0..=W).contains(x)));
        assert!(svg.contains("&lt;Paso&gt;") && !svg.contains("<Paso>"));
        assert!(render(&[], &[], &[], "x").is_none());
        let one = render(&[pt(35.3, -120.6, "a")], &[], &[], "x").unwrap();
        assert!(
            circles(&one)[0].0 > 300.0 && circles(&one)[0].0 < 340.0,
            "centered"
        );
    }
}
