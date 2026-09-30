//! Kamera-Zustand (Rotation, Idle-Timeout) und reine 3D→2D-Projektion.
//!
//! Bewusst frei von `egui`-Typen: alle Funktionen hier sind reine
//! Mathematik mit festen Eingaben/Ausgaben, damit sie ohne GUI-Kontext mit
//! festen Fixtures testbar sind (Regel 24). Die Umrechnung in
//! `egui::Pos2`/`egui::Painter`-Aufrufe passiert erst in `mod.rs`.

use super::layout::Vec3;

/// Rotiert `p` zuerst um die Y-Achse (`yaw`, horizontale Drehung -- das ist
/// die im Auftrag geforderte automatische Rotation), danach leicht um die
/// X-Achse (`pitch`, nur per Maus-Drag erreichbar, siehe
/// [`clamp_pitch`]).
pub fn rotate_yaw_pitch(p: Vec3, yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let x1 = p.x * cy + p.z * sy;
    let z1 = -p.x * sy + p.z * cy;

    let (sp, cp) = pitch.sin_cos();
    let y2 = p.y * cp - z1 * sp;
    let z2 = p.y * sp + z1 * cp;

    Vec3::new(x1, y2, z2)
}

/// Maximale Neigung nach oben/unten per Drag, damit der Graph nie ganz auf
/// den Kopf gedreht werden kann (das würde bei einer reinen
/// Horizontal-Rotation-Anmutung eher verwirren als nützen).
pub const MAX_PITCH_RAD: f32 = 0.6; // ≈ 34°

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Rotation um die Y-Achse in Radiant, 0..2π (wird gewrappt, damit der
    /// Wert bei Dauerbetrieb nicht unbegrenzt wächst).
    pub yaw: f32,
    pub pitch: f32,
    /// Abstand der Kamera vom Ursprung entlang -Z.
    pub distance: f32,
    /// Brennweite für die perspektivische Projektion (größer = weniger
    /// Verzerrung/Zoom).
    pub focal_length: f32,
    /// Gleichmäßiger 2D-Zoomfaktor auf das projizierte Bild (1.0 = Start).
    /// Getrennt vom Kamera-`distance`, damit das Zoomen auf den Mauszeiger
    /// zentriert werden kann (siehe [`Camera::zoom_at`]).
    pub zoom: f32,
    /// Bildschirm-Verschiebung (Pixel) relativ zur Mitte des Zeichenbereichs.
    pub pan_x: f32,
    pub pan_y: f32,
}

/// Grenzen des 2D-Zooms: weit genug heraus für den ganzen Graphen, weit
/// genug hinein, um einzelne Knoten zu lesen.
pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 12.0;

impl Camera {
    pub fn new(distance: f32, focal_length: f32) -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            distance,
            focal_length,
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    /// Zoomt um `factor` und hält dabei den Bildpunkt unter dem Mauszeiger
    /// (`rel_x`/`rel_y` in Pixeln relativ zur Mitte des Zeichenbereichs)
    /// an derselben Stelle.
    pub fn zoom_at(&mut self, rel_x: f32, rel_y: f32, factor: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = new_zoom / self.zoom;
        self.pan_x = rel_x - (rel_x - self.pan_x) * ratio;
        self.pan_y = rel_y - (rel_y - self.pan_y) * ratio;
        self.zoom = new_zoom;
    }

    /// Verschiebt die Ansicht um `dx`/`dy` Pixel.
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan_x += dx;
        self.pan_y += dy;
    }

    /// Setzt Zoom und Verschiebung zurück.
    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }

    /// Führt die automatische horizontale Rotation um `degrees_per_sec *
    /// dt_secs` fort.
    pub fn advance_auto_rotation(&mut self, degrees_per_sec: f32, dt_secs: f32) {
        self.yaw = wrap_angle(self.yaw + degrees_per_sec.to_radians() * dt_secs);
    }

    /// Wendet eine Maus-Drag-Bewegung an: horizontal auf `yaw`, vertikal
    /// (invertiert, wie in 3D-Viewern üblich: nach oben ziehen kippt die
    /// Ansicht nach oben) auf `pitch`, mit harter Begrenzung.
    pub fn apply_drag(&mut self, delta_x: f32, delta_y: f32, sensitivity_deg_per_px: f32) {
        self.yaw = wrap_angle(self.yaw + (delta_x * sensitivity_deg_per_px).to_radians());
        self.pitch = (self.pitch - (delta_y * sensitivity_deg_per_px).to_radians())
            .clamp(-MAX_PITCH_RAD, MAX_PITCH_RAD);
    }
}

fn wrap_angle(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let wrapped = angle % tau;
    if wrapped < 0.0 {
        wrapped + tau
    } else {
        wrapped
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projected {
    /// Bildschirm-x/y, zentriert auf (0, 0) -- der Aufrufer verschiebt dies
    /// noch auf den Mittelpunkt des Zeichenbereichs.
    pub x: f32,
    pub y: f32,
    /// Tiefe im Kamera-Raum (größer = weiter weg), zum Sortieren fürs
    /// Malerprinzip und zur größenabhängigen Skalierung von Knoten.
    pub depth: f32,
}

/// Projiziert einen 3D-Punkt unter der gegebenen Kamera-Rotation in den
/// Bildschirmraum. Liefert `None`, wenn der Punkt hinter oder zu nah an der
/// Kamera liegt (Regel 16: `None` statt Division durch (nahe) Null).
pub fn project_point(p: Vec3, camera: &Camera) -> Option<Projected> {
    let rotated = rotate_yaw_pitch(p, camera.yaw, camera.pitch);
    let depth = rotated.z + camera.distance;
    const NEAR_CLIP: f32 = 1.0;
    if depth <= NEAR_CLIP {
        return None;
    }
    let scale = camera.focal_length / depth;
    Some(Projected {
        x: rotated.x * scale,
        y: rotated.y * scale,
        depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_um_null_grad_ist_identitaet() {
        let p = Vec3::new(1.0, 2.0, 3.0);
        let rotated = rotate_yaw_pitch(p, 0.0, 0.0);
        assert!((rotated.x - p.x).abs() < 1e-5);
        assert!((rotated.y - p.y).abs() < 1e-5);
        assert!((rotated.z - p.z).abs() < 1e-5);
    }

    #[test]
    fn yaw_180_grad_spiegelt_x_und_z() {
        let p = Vec3::new(1.0, 0.0, 0.0);
        let rotated = rotate_yaw_pitch(p, std::f32::consts::PI, 0.0);
        assert!((rotated.x - (-1.0)).abs() < 1e-4, "x war {}", rotated.x);
        assert!(rotated.z.abs() < 1e-4, "z war {}", rotated.z);
    }

    #[test]
    fn projektion_von_ursprung_liegt_in_bildschirmmitte() {
        let camera = Camera::new(500.0, 800.0);
        let projected = project_point(Vec3::ZERO, &camera).expect("Ursprung ist sichtbar");
        assert!(projected.x.abs() < 1e-4);
        assert!(projected.y.abs() < 1e-4);
        assert!((projected.depth - 500.0).abs() < 1e-4);
    }

    #[test]
    fn punkt_hinter_der_kamera_wird_nicht_projiziert() {
        let camera = Camera::new(100.0, 800.0);
        // z = -200 liegt (nach Rotation um 0) bei depth = -200 + 100 = -100 < NEAR_CLIP.
        let behind = Vec3::new(0.0, 0.0, -200.0);
        assert!(project_point(behind, &camera).is_none());
    }

    #[test]
    fn naeherer_punkt_projiziert_groesser() {
        let camera = Camera::new(500.0, 800.0);
        let far = project_point(Vec3::new(50.0, 0.0, 100.0), &camera).unwrap();
        let near = project_point(Vec3::new(50.0, 0.0, -100.0), &camera).unwrap();
        assert!(
            near.x.abs() > far.x.abs(),
            "naeherer Punkt sollte weiter aussen liegen"
        );
    }

    #[test]
    fn zoom_at_haelt_punkt_unter_dem_mauszeiger_fest() {
        let mut camera = Camera::new(500.0, 800.0);
        camera.pan_by(30.0, -20.0);
        // Bildschirmposition eines projizierten Punktes v vor/nach dem Zoom.
        let v = (40.0_f32, 25.0_f32);
        let screen = |c: &Camera| (c.pan_x + c.zoom * v.0, c.pan_y + c.zoom * v.1);
        let (sx, sy) = screen(&camera);
        camera.zoom_at(sx, sy, 2.5);
        let (nx, ny) = screen(&camera);
        assert!((nx - sx).abs() < 1e-3 && (ny - sy).abs() < 1e-3, "{nx},{ny} vs {sx},{sy}");
        assert!((camera.zoom - 2.5).abs() < 1e-5);
    }

    #[test]
    fn zoom_wird_begrenzt_und_reset_stellt_ursprung_her() {
        let mut camera = Camera::new(500.0, 800.0);
        camera.zoom_at(10.0, 10.0, 1000.0);
        assert!((camera.zoom - MAX_ZOOM).abs() < 1e-5);
        camera.zoom_at(10.0, 10.0, 1e-6);
        assert!((camera.zoom - MIN_ZOOM).abs() < 1e-5);
        camera.reset_view();
        assert_eq!((camera.zoom, camera.pan_x, camera.pan_y), (1.0, 0.0, 0.0));
    }

    #[test]
    fn advance_auto_rotation_wraps_in_0_bis_tau() {
        let mut camera = Camera::new(500.0, 800.0);
        camera.yaw = std::f32::consts::TAU - 0.01;
        camera.advance_auto_rotation(180.0, 1.0); // +π, sollte über TAU hinaus wrappen
        assert!(camera.yaw >= 0.0 && camera.yaw < std::f32::consts::TAU);
    }

    #[test]
    fn apply_drag_begrenzt_pitch() {
        let mut camera = Camera::new(500.0, 800.0);
        camera.apply_drag(0.0, 100_000.0, 1.0);
        assert!(camera.pitch >= -MAX_PITCH_RAD - 1e-4);
        camera.apply_drag(0.0, -100_000.0, 1.0);
        assert!(camera.pitch <= MAX_PITCH_RAD + 1e-4);
    }
}
