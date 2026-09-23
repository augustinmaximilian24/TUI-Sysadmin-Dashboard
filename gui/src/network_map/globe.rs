//! Reine 3D-Mathematik für die drehbare Weltkugel: lat/lon auf der
//! Einheitskugel, Yaw/Pitch-Rotation, orthografische Projektion und
//! Rückseiten-Sichtbarkeit. Bewusst frei von `egui`-Typen (wie
//! [`crate::knowledge_graph::camera`], nach demselben Muster: Drag rotiert,
//! nach Leerlauf zieht die Kamera zurück), aber eigenständig statt an das
//! unverwandte Wissensgraph-Feature gekoppelt -- beide Module haben
//! zufällig dieselbe Kameramechanik nötig, sonst nichts gemeinsam.

/// Punkt auf der Einheitskugel (Radius 1) in Kamera-Raum-Koordinaten.
#[derive(Debug, Clone, Copy)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Wandelt geografische Koordinaten in einen Punkt auf der Einheitskugel um.
/// Vorzeichen von `z` bewusst so gewählt, dass `lon=0, lat=0` bei
/// `yaw=0, pitch=0` **zur Kamera zeigt** (siehe [`GlobeCamera::centered_on`])
/// -- nicht die mathematisch "übliche" Konvention, aber die, die eine
/// Kamera ohne zusätzliche 180°-Drehung direkt nutzbar macht.
pub fn lat_lon_to_unit_sphere(lat_deg: f32, lon_deg: f32) -> Vec3 {
    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();
    Vec3::new(lat.cos() * lon.sin(), lat.sin(), -(lat.cos() * lon.cos()))
}

/// Dieselbe Rotationsreihenfolge (erst Yaw um die Y-, dann Pitch um die
/// X-Achse) wie [`crate::knowledge_graph::camera::rotate_yaw_pitch`] --
/// unabhängig neu geschrieben statt importiert, damit `network_map` nicht
/// von einem inhaltlich unverwandten Feature abhängt, aber mit denselben,
/// dort bereits getesteten Formeln.
fn rotate_yaw_pitch(p: Vec3, yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let x1 = p.x * cy + p.z * sy;
    let z1 = -p.x * sy + p.z * cy;

    let (sp, cp) = pitch.sin_cos();
    let y2 = p.y * cp - z1 * sp;
    let z2 = p.y * sp + z1 * cp;

    Vec3::new(x1, y2, z2)
}

/// Maximale Neigung nach oben/unten per Drag -- verhindert, dass die Kugel
/// über den Pol hinaus auf den Kopf gedreht wird.
pub const MAX_PITCH_RAD: f32 = 1.3; // ≈ 74°

/// Ein bereits (yaw/pitch-)rotierter Punkt, noch unprojiziert. `visible`
/// ist `true`, wenn er auf der der Kamera zugewandten Hälfte der Kugel
/// liegt (`z <= 0`, siehe [`lat_lon_to_unit_sphere`]).
#[derive(Debug, Clone, Copy)]
pub struct Rotated {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Rotated {
    pub fn visible(&self) -> bool {
        self.z <= 0.0
    }
}

/// Erlaubter Zoom-Bereich als Faktor auf den sonst panelfüllenden
/// Kugelradius -- 1.0 ist die bisherige, unveränderte Größe. Nach unten
/// begrenzt, damit die Kugel nicht zum unlesbaren Punkt schrumpft, nach
/// oben, damit sie nicht so weit über den Panelrand hinauswächst, dass gar
/// keine Orientierung mehr möglich ist (der Rand clippt ohnehin, siehe
/// `ui.painter_at` in `mod.rs`).
pub const MIN_ZOOM: f32 = 0.5;
pub const MAX_ZOOM: f32 = 4.0;

/// Wie schnell die freie Drehung nach einem "Wurf" (Maus mit Schwung
/// loslassen) abklingt -- exponentielle Dämpfung pro Sekunde, im selben
/// Muster wie [`GlobeCamera::ease_towards_origin`].
const MOMENTUM_DAMPING_PER_SECOND: f32 = 2.2;

/// Unterhalb dieser Geschwindigkeit (Grad/Sekunde) gilt die freie Drehung
/// als ausgeklungen -- ohne Schwellwert würde sie asymptotisch nie ganz auf
/// null fallen und der Leerlauf-Timer für die Rückkehr zur Ursprungsansicht
/// nie anlaufen.
const MOMENTUM_STOP_THRESHOLD_DEG_PER_SEC: f32 = 1.0;

/// Kamera-Zustand für die drehbare Kugel: aktuelle Ausrichtung/Zoom plus die
/// "Ursprungsansicht", zu der nach einer Leerlaufzeit zurückgekehrt wird,
/// sowie die Dreh-Geschwindigkeit für das Nachdrehen nach einem "Wurf".
#[derive(Debug, Clone, Copy)]
pub struct GlobeCamera {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    default_yaw: f32,
    default_pitch: f32,
    yaw_velocity: f32,
    pitch_velocity: f32,
}

impl GlobeCamera {
    /// Ursprungsansicht so gewählt, dass `(lat_deg, lon_deg)` (typischerweise
    /// der "Zuhause"-Punkt) direkt zur Kamera zeigt: hergeleitet aus
    /// `rotate_yaw_pitch` durch Winkeladditionstheoreme -- `yaw = lon`,
    /// `pitch = -lat` bringt den Punkt exakt auf `(x=0, y=0, z=-1)`, die
    /// Bildschirmmitte zugewandt zur Kamera.
    pub fn centered_on(lat_deg: f32, lon_deg: f32) -> Self {
        let yaw = lon_deg.to_radians();
        let pitch = (-lat_deg).to_radians().clamp(-MAX_PITCH_RAD, MAX_PITCH_RAD);
        Self {
            yaw,
            pitch,
            zoom: 1.0,
            default_yaw: yaw,
            default_pitch: pitch,
            yaw_velocity: 0.0,
            pitch_velocity: 0.0,
        }
    }

    pub fn rotate(&self, p: Vec3) -> Rotated {
        let r = rotate_yaw_pitch(p, self.yaw, self.pitch);
        Rotated { x: r.x, y: r.y, z: r.z }
    }

    /// Wendet eine Maus-Drag-Bewegung an -- horizontal (invertiert: nach
    /// rechts ziehen dreht die Kugel so, dass ihre Oberfläche der
    /// Mausbewegung folgt, wie man eine Kugel mit der Hand dreht statt wie
    /// eine Kamera um sie herumzuführen) auf `yaw`, vertikal (ebenfalls
    /// invertiert, wie in 3D-Viewern üblich) auf `pitch`, mit fester
    /// Begrenzung gegen Überschlag über den Pol.
    pub fn apply_drag(&mut self, delta_x: f32, delta_y: f32, sensitivity_deg_per_px: f32) {
        self.yaw = wrap_angle(self.yaw - (delta_x * sensitivity_deg_per_px).to_radians());
        self.pitch = (self.pitch - (delta_y * sensitivity_deg_per_px).to_radians())
            .clamp(-MAX_PITCH_RAD, MAX_PITCH_RAD);
    }

    /// Merkt sich die aktuelle Zieh-Geschwindigkeit (Pixel/Sekunde, gleiche
    /// Vorzeichen-/Skalierungslogik wie [`Self::apply_drag`]) -- Grundlage
    /// für das Nachdrehen in [`Self::apply_momentum`], sobald die Maus
    /// losgelassen wird, während sie noch in Bewegung war.
    pub fn set_drag_velocity(&mut self, delta_x_per_sec: f32, delta_y_per_sec: f32, sensitivity_deg_per_px: f32) {
        self.yaw_velocity = -(delta_x_per_sec * sensitivity_deg_per_px).to_radians();
        self.pitch_velocity = -(delta_y_per_sec * sensitivity_deg_per_px).to_radians();
    }

    /// `true`, solange die freie Drehung nach einem "Wurf" noch spürbar
    /// weiterläuft (siehe [`MOMENTUM_STOP_THRESHOLD_DEG_PER_SEC`]).
    pub fn has_momentum(&self) -> bool {
        self.yaw_velocity.to_degrees().abs() > MOMENTUM_STOP_THRESHOLD_DEG_PER_SEC
            || self.pitch_velocity.to_degrees().abs() > MOMENTUM_STOP_THRESHOLD_DEG_PER_SEC
    }

    /// Dreht die Kugel für einen Frame mit der zuletzt gemerkten
    /// Geschwindigkeit weiter und dämpft diese exponentiell -- lässt eine
    /// zügige Dreh-Geste nach dem Loslassen sanft ausklingen statt abrupt
    /// zu stoppen, was sich beim Drehen deutlich flüssiger anfühlt.
    pub fn apply_momentum(&mut self, dt: f32) {
        self.yaw = wrap_angle(self.yaw + self.yaw_velocity * dt);
        self.pitch =
            (self.pitch + self.pitch_velocity * dt).clamp(-MAX_PITCH_RAD, MAX_PITCH_RAD);
        let decay = (-MOMENTUM_DAMPING_PER_SECOND * dt).exp();
        self.yaw_velocity *= decay;
        self.pitch_velocity *= decay;
    }

    /// Wendet Scrollen als Zoom an, begrenzt auf [`MIN_ZOOM`]/[`MAX_ZOOM`] --
    /// dieselbe Grundidee wie die Abstands-Zoomsteuerung des
    /// Wissensgraph-Tabs (`knowledge_graph::Camera`), hier als Faktor auf
    /// den Kugelradius statt als Kamera-Abstand, weil die Kugel
    /// orthografisch statt perspektivisch projiziert wird.
    pub fn apply_zoom(&mut self, scroll_delta_y: f32, sensitivity: f32) {
        self.zoom = (self.zoom + scroll_delta_y * sensitivity).clamp(MIN_ZOOM, MAX_ZOOM);
    }

    /// Zieht `yaw`/`pitch` frame-raten-unabhängig exponentiell zurück zur
    /// Ursprungsansicht (dasselbe Glättungsmuster wie bei der 2D-Kamera vor
    /// dieser Umstellung: `1 - exp(-ease*dt)`).
    pub fn ease_towards_origin(&mut self, ease_per_second: f32, dt: f32) {
        let t = 1.0 - (-ease_per_second * dt).exp();
        // Kürzesten Weg über den ±180°-Sprung nehmen, sonst dreht sich die
        // Kugel bei einem Ursprung nahe der Datumsgrenze einmal unnötig
        // ganz herum.
        let mut delta_yaw = self.default_yaw - self.yaw;
        delta_yaw = delta_yaw.rem_euclid(std::f32::consts::TAU);
        if delta_yaw > std::f32::consts::PI {
            delta_yaw -= std::f32::consts::TAU;
        }
        self.yaw = wrap_angle(self.yaw + delta_yaw * t);
        self.pitch += (self.default_pitch - self.pitch) * t;
    }
}

/// Hält einen Winkel im Bereich `[0, 2π)` -- ohne das würde `yaw` bei
/// dauerhaftem Betrieb (viele kleine Drags über Tage) unbegrenzt wachsen
/// und irgendwann an Gleitkomma-Präzision verlieren.
fn wrap_angle(angle: f32) -> f32 {
    angle.rem_euclid(std::f32::consts::TAU)
}

/// Sphärische lineare Interpolation zwischen zwei Punkten auf der
/// Einheitskugel -- ergibt den kürzesten Weg über die Kugeloberfläche
/// (Großkreis) statt der Luftlinie durchs Innere. Fällt bei (fast)
/// identischen oder exakt entgegengesetzten Punkten (Winkel ≈ 0 bzw. ≈ π,
/// dort ist die Großkreis-Richtung nicht eindeutig) auf lineares
/// Interpolieren zurück statt durch fast Null zu teilen.
fn slerp(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    let dot = (a.x * b.x + a.y * b.y + a.z * b.z).clamp(-1.0, 1.0);
    let omega = dot.acos();
    if omega.abs() < 1e-4 || (std::f32::consts::PI - omega).abs() < 1e-4 {
        return Vec3::new(
            a.x + (b.x - a.x) * t,
            a.y + (b.y - a.y) * t,
            a.z + (b.z - a.z) * t,
        );
    }
    let sin_omega = omega.sin();
    let wa = ((1.0 - t) * omega).sin() / sin_omega;
    let wb = (t * omega).sin() / sin_omega;
    Vec3::new(wa * a.x + wb * b.x, wa * a.y + wb * b.y, wa * a.z + wb * b.z)
}

/// Punkt auf einem Verbindungsbogen zwischen `a` und `b`, beide auf der
/// Einheitskugel: folgt dem Großkreis ([`slerp`]), angehoben um `bulge`
/// (als Anteil des Kugelradius) am höchsten Punkt der Kurve -- derselbe
/// "Flugroute"-Look wie viele Netzwerk-Weltkarten, damit sich Start und
/// Ziel optisch von der reinen Landmasse abheben.
pub fn arc_point(a: Vec3, b: Vec3, t: f32, bulge: f32) -> Vec3 {
    let base = slerp(a, b, t);
    let lift = 1.0 + bulge * (std::f32::consts::PI * t).sin();
    Vec3::new(base.x * lift, base.y * lift, base.z * lift)
}

/// Wölbung eines Routenabschnitts, anteilig zu seiner Großkreislänge
/// (`max_bulge` bei genau gegenüberliegenden Punkten).
///
/// Ohne diese Abstufung würde ein kurzer Abschnitt (Zuhause -> nächste
/// Stadt) genauso hoch aufgewölbt wie ein transatlantischer und die
/// gezeichnete Route damit einen Weg vortäuschen, der mit der tatsächlich
/// zurückgelegten Strecke nichts zu tun hat.
pub fn segment_bulge(a: Vec3, b: Vec3, max_bulge: f32) -> f32 {
    let dot = (a.x * b.x + a.y * b.y + a.z * b.z).clamp(-1.0, 1.0);
    max_bulge * (dot.acos() / std::f32::consts::PI)
}

/// Orthografische Projektion: Kugelkoordinaten liegen bereits in [-1, 1],
/// werden nur noch mit dem Bildschirmradius skaliert und auf die
/// Bildmitte verschoben -- kein Blickfeld-/Brennweiten-Tuning nötig, und
/// die Kugel füllt bei jeder Panelgröße exakt einen Kreis mit Radius
/// `radius_px`.
fn project(r: Rotated, center_x: f32, center_y: f32, radius_px: f32) -> (f32, f32) {
    (center_x + r.x * radius_px, center_y - r.y * radius_px)
}

/// Bündelt Kamera plus Bildschirm-Mittelpunkt/-Radius -- vermeidet, dass
/// jeder Aufrufer diese drei Werte einzeln durchreichen muss (frühere
/// Fassung hatte deswegen eine Funktion mit sieben Parametern, siehe
/// Commit-Historie).
#[derive(Debug, Clone, Copy)]
pub struct GlobeView {
    pub camera: GlobeCamera,
    pub center: (f32, f32),
    pub radius_px: f32,
}

impl GlobeView {
    /// Rotiert und projiziert einen Kugelpunkt in Bildschirmkoordinaten.
    /// `visible` folgt [`Rotated::visible`] -- der Aufrufer entscheidet,
    /// ob er unsichtbare Punkte trotzdem (z. B. für einen abbrechenden
    /// Verbindungsbogen) oder gar nicht zeichnet.
    pub fn project(&self, p: Vec3) -> (f32, f32, bool) {
        let rotated = self.camera.rotate(p);
        let (x, y) = project(rotated, self.center.0, self.center.1, self.radius_px);
        (x, y, rotated.visible())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lon_lat_null_zeigt_bei_neutraler_kamera_zur_kamera() {
        let p = lat_lon_to_unit_sphere(0.0, 0.0);
        assert!((p.x).abs() < 1e-5);
        assert!((p.y).abs() < 1e-5);
        assert!((p.z - (-1.0)).abs() < 1e-5, "z war {}", p.z);
    }

    #[test]
    fn centered_on_bringt_den_zielpunkt_exakt_zur_kamera() {
        let camera = GlobeCamera::centered_on(51.0, 9.0); // ungefähr Deutschland
        let point = lat_lon_to_unit_sphere(51.0, 9.0);
        let rotated = camera.rotate(point);
        assert!(rotated.x.abs() < 1e-4, "x war {}", rotated.x);
        assert!(rotated.y.abs() < 1e-4, "y war {}", rotated.y);
        assert!((rotated.z - (-1.0)).abs() < 1e-4, "z war {}", rotated.z);
    }

    #[test]
    fn punkt_auf_der_gegenseite_ist_nicht_sichtbar() {
        let camera = GlobeCamera::centered_on(0.0, 0.0);
        let antipode = lat_lon_to_unit_sphere(0.0, 180.0);
        let rotated = camera.rotate(antipode);
        assert!(!rotated.visible());
    }

    #[test]
    fn arc_point_beginnt_und_endet_exakt_bei_a_und_b() {
        let a = lat_lon_to_unit_sphere(50.0, 8.0);
        let b = lat_lon_to_unit_sphere(40.0, -100.0);
        let start = arc_point(a, b, 0.0, 0.2);
        let end = arc_point(a, b, 1.0, 0.2);
        assert!((start.x - a.x).abs() < 1e-4 && (start.y - a.y).abs() < 1e-4 && (start.z - a.z).abs() < 1e-4);
        assert!((end.x - b.x).abs() < 1e-4 && (end.y - b.y).abs() < 1e-4 && (end.z - b.z).abs() < 1e-4);
    }

    #[test]
    fn arc_point_woelbt_sich_in_der_mitte_ueber_die_kugel() {
        let a = lat_lon_to_unit_sphere(0.0, -30.0);
        let b = lat_lon_to_unit_sphere(0.0, 30.0);
        let mid = arc_point(a, b, 0.5, 0.2);
        let dist = (mid.x * mid.x + mid.y * mid.y + mid.z * mid.z).sqrt();
        assert!(dist > 1.05, "Bogenmitte sollte über die Kugeloberfläche hinausragen, war {dist}");
    }

    #[test]
    fn slerp_liefert_bei_identischen_punkten_denselben_punkt() {
        let a = lat_lon_to_unit_sphere(12.0, 34.0);
        let result = slerp(a, a, 0.5);
        assert!((result.x - a.x).abs() < 1e-4);
        assert!((result.y - a.y).abs() < 1e-4);
        assert!((result.z - a.z).abs() < 1e-4);
    }

    #[test]
    fn apply_drag_begrenzt_pitch() {
        let mut camera = GlobeCamera::centered_on(0.0, 0.0);
        camera.apply_drag(0.0, 100_000.0, 1.0);
        assert!(camera.pitch >= -MAX_PITCH_RAD - 1e-4);
        camera.apply_drag(0.0, -100_000.0, 1.0);
        assert!(camera.pitch <= MAX_PITCH_RAD + 1e-4);
    }

    #[test]
    fn ease_towards_origin_konvergiert_zur_ursprungsansicht() {
        let mut camera = GlobeCamera::centered_on(10.0, 20.0);
        camera.apply_drag(500.0, 300.0, 1.0);
        for _ in 0..200 {
            camera.ease_towards_origin(3.0, 1.0 / 30.0);
        }
        assert!((camera.yaw - 20.0_f32.to_radians()).abs() < 1e-3);
        assert!((camera.pitch - (-10.0_f32.to_radians())).abs() < 1e-3);
    }

    #[test]
    fn ease_towards_origin_nimmt_kuerzesten_weg_ueber_datumsgrenze() {
        // Ursprung bei 179°, aktuelle Ausrichtung leicht jenseits von -179°
        // (also nur 2° entfernt, nicht 358°) -- nach einem einzigen kleinen
        // Schritt darf sich `yaw` nur um wenige Grad bewegt haben, nicht
        // fast einmal komplett herum.
        let mut camera = GlobeCamera::centered_on(0.0, 179.0);
        camera.yaw = wrap_angle((-179.0_f32).to_radians());
        let before = camera.yaw;
        camera.ease_towards_origin(3.0, 1.0 / 30.0);
        // Kürzeste Winkeldistanz statt roher Differenz messen: `yaw` wird
        // jetzt (wie `apply_drag`) nach jedem Schritt in [0, 2π) gehalten,
        // eine rohe Differenz über den Wrap-Punkt hinweg (z. B. 359° ->
        // 1°) sähe sonst wie ein fast voller Umlauf aus, obwohl die
        // tatsächliche Drehung winzig war.
        let mut moved = (camera.yaw - before).rem_euclid(std::f32::consts::TAU);
        if moved > std::f32::consts::PI {
            moved = std::f32::consts::TAU - moved;
        }
        assert!(moved < 10.0_f32.to_radians(), "yaw sprang um {} rad", moved);
    }

    #[test]
    fn segment_bulge_waechst_mit_der_segmentlaenge() {
        let a = lat_lon_to_unit_sphere(50.0, 8.0);
        let nah = lat_lon_to_unit_sphere(48.0, 11.0); // Frankfurt -> München
        let fern = lat_lon_to_unit_sphere(37.0, -122.0); // -> Kalifornien
        let kurz = segment_bulge(a, nah, 0.18);
        let lang = segment_bulge(a, fern, 0.18);
        assert!(kurz < lang, "kurzes Segment {kurz} sollte flacher sein als langes {lang}");
        assert!(kurz < 0.02, "wenige hundert Kilometer dürfen kaum wölben, war {kurz}");
        assert!(lang <= 0.18);
    }

    #[test]
    fn segment_bulge_ist_bei_identischen_punkten_praktisch_null() {
        // Nicht exakt null: `acos` verliert in `f32` nahe 1.0 Genauigkeit
        // (dort ist die Ableitung unendlich), übrig bleiben ~2e-4 rad.
        // Als Wölbung sind das rund 0,002 % des Kugelradius, also weit
        // unter einem Pixel -- hier bewusst toleriert, statt die Funktion
        // um eine Sonderbehandlung ohne sichtbare Wirkung zu erweitern.
        let a = lat_lon_to_unit_sphere(10.0, 20.0);
        assert!(segment_bulge(a, a, 0.18).abs() < 1e-3);
    }

    #[test]
    fn apply_zoom_begrenzt_auf_min_und_max() {
        let mut camera = GlobeCamera::centered_on(0.0, 0.0);
        camera.apply_zoom(-1000.0, 1.0);
        assert!((camera.zoom - MIN_ZOOM).abs() < 1e-4);
        camera.apply_zoom(1000.0, 1.0);
        assert!((camera.zoom - MAX_ZOOM).abs() < 1e-4);
    }

    #[test]
    fn momentum_klingt_ueber_zeit_ab_und_dreht_die_kugel() {
        let mut camera = GlobeCamera::centered_on(0.0, 0.0);
        camera.set_drag_velocity(500.0, 0.0, 1.0);
        assert!(camera.has_momentum());
        let yaw_before = camera.yaw;
        for _ in 0..200 {
            camera.apply_momentum(1.0 / 60.0);
        }
        assert!(!camera.has_momentum(), "Schwung sollte nach 200 Frames ausgeklungen sein");
        assert!((camera.yaw - yaw_before).abs() > 1e-3, "Kugel sollte sich durch den Schwung gedreht haben");
    }

    #[test]
    fn projektion_skaliert_und_zentriert() {
        let (x, y) = project(Rotated { x: 1.0, y: 1.0, z: -1.0 }, 100.0, 100.0, 50.0);
        assert!((x - 150.0).abs() < 1e-4);
        assert!((y - 50.0).abs() < 1e-4); // y invertiert: Bildschirm-y wächst nach unten
    }
}
