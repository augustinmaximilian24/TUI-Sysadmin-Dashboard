//! Deterministisches 3D-Force-Layout für den Wissensgraphen.
//!
//! `graph.json` enthält keine Positionen (graphify berechnet sein 2D-Layout
//! per Force-Simulation clientseitig in `graph.html`, siehe
//! `_reconcile_graph_html`/`to_html` im graphify-Quellcode) -- hier
//! entsprechend eine einmalige, reine 3D-Variante: Fibonacci-Kugel als
//! Startpositionen (deterministisch, kollisionsarm), danach klassisches
//! Feder-Abstoßungs-Layout (Fruchterman-Reingold-artig) über
//! `LayoutParams::iterations` Schritte. Läuft nur beim (Neu-)Laden eines
//! Graphen im Hintergrund-Thread, nicht pro Frame (Regel 21).

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn add(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }

    pub fn sub(self, other: Vec3) -> Vec3 {
        Vec3::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    pub fn scale(self, factor: f32) -> Vec3 {
        Vec3::new(self.x * factor, self.y * factor, self.z * factor)
    }

    pub fn length_squared(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Normiert auf Länge 1. Bei (nahezu) Nulllänge wird bewusst
    /// `Vec3::ZERO` statt eines `panic!`/`NaN`-Vektors zurückgegeben (Regel
    /// 16: kein Absturz im Berechnungspfad), da zwei exakt deckungsgleiche
    /// Punkte im Layout ohnehin keine sinnvolle Richtung ergeben.
    pub fn normalized(self) -> Vec3 {
        let len = self.length();
        if len < 1e-6 {
            Vec3::ZERO
        } else {
            self.scale(1.0 / len)
        }
    }
}

/// Platziert Punkt `i` von `n` gleichmäßig auf einer Kugel mit `radius`
/// (Fibonacci-Kugel: goldener Winkel längs der y-Achse). Deterministisch --
/// derselbe Graph ergibt immer dieselbe Startaufstellung, keine Zufallszahl
/// nötig.
pub fn fibonacci_sphere_point(i: usize, n: usize, radius: f32) -> Vec3 {
    if n <= 1 {
        return Vec3::ZERO;
    }
    const GOLDEN_ANGLE: f32 = 2.399_963_2; // π · (3 − √5)
    let y = 1.0 - (i as f32 / (n - 1) as f32) * 2.0; // 1 .. -1
    let radius_at_y = (1.0 - y * y).max(0.0).sqrt();
    let theta = GOLDEN_ANGLE * i as f32;
    Vec3::new(
        theta.cos() * radius_at_y * radius,
        y * radius,
        theta.sin() * radius_at_y * radius,
    )
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutParams {
    pub iterations: usize,
    /// Stärke der paarweisen Abstoßung zwischen allen Knoten.
    pub repulsion: f32,
    /// Ziel-Kantenlänge, auf die die Federkraft hinarbeitet.
    pub spring_length: f32,
    pub spring_strength: f32,
    /// Dämpfung pro Iteration (0..1): begrenzt, wie stark eine einzelne
    /// Kraft die Position in einem Schritt verschiebt.
    pub damping: f32,
    /// Harte Obergrenze der Verschiebung pro Knoten und Iteration, damit
    /// ein anfangs sehr naher Knotenpaar das Layout nicht "explodieren"
    /// lässt.
    pub max_step: f32,
    /// Startradius der Fibonacci-Kugel.
    pub initial_radius: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            iterations: 300,
            repulsion: 4_000.0,
            spring_length: 60.0,
            spring_strength: 0.06,
            damping: 0.4,
            max_step: 20.0,
            initial_radius: 200.0,
        }
    }
}

impl LayoutParams {
    pub fn with_iterations(iterations: usize) -> Self {
        Self {
            iterations,
            ..Default::default()
        }
    }
}

/// Berechnet 3D-Positionen für `node_count` Knoten unter Berücksichtigung
/// von `edges` (Indexpaare in `0..node_count`). Ungültige Indizes werden
/// übersprungen statt zu einem Absturz zu führen (Regel 16) -- der Aufrufer
/// filtert Kanten mit unbekannter Knoten-ID bereits vorher heraus, das ist
/// hier nur eine zweite Absicherung.
pub fn layout_3d(node_count: usize, edges: &[(usize, usize)], params: &LayoutParams) -> Vec<Vec3> {
    let mut positions: Vec<Vec3> = (0..node_count)
        .map(|i| fibonacci_sphere_point(i, node_count, params.initial_radius))
        .collect();

    if node_count < 2 {
        return positions;
    }

    for _ in 0..params.iterations {
        let mut forces = vec![Vec3::ZERO; node_count];

        for i in 0..node_count {
            for j in (i + 1)..node_count {
                let delta = positions[i].sub(positions[j]);
                let dist_sq = delta.length_squared().max(1.0);
                let dir = delta.normalized();
                let magnitude = params.repulsion / dist_sq;
                forces[i] = forces[i].add(dir.scale(magnitude));
                forces[j] = forces[j].sub(dir.scale(magnitude));
            }
        }

        for &(a, b) in edges {
            if a == b || a >= node_count || b >= node_count {
                continue;
            }
            let delta = positions[b].sub(positions[a]);
            let dist = delta.length().max(0.001);
            let dir = delta.scale(1.0 / dist);
            let displacement = dist - params.spring_length;
            let force = dir.scale(displacement * params.spring_strength);
            forces[a] = forces[a].add(force);
            forces[b] = forces[b].sub(force);
        }

        for i in 0..node_count {
            let mut step = forces[i].scale(params.damping);
            let step_len = step.length();
            if step_len > params.max_step {
                step = step.normalized().scale(params.max_step);
            }
            positions[i] = positions[i].add(step);
        }
    }

    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fibonacci_sphere_liegt_auf_erwartetem_radius() {
        for i in 0..10 {
            let p = fibonacci_sphere_point(i, 10, 100.0);
            assert!(
                (p.length() - 100.0).abs() < 0.01,
                "Punkt {i} hat Länge {}",
                p.length()
            );
        }
    }

    #[test]
    fn fibonacci_sphere_mit_einem_knoten_liegt_im_ursprung() {
        assert_eq!(fibonacci_sphere_point(0, 1, 100.0), Vec3::ZERO);
    }

    #[test]
    fn layout_mit_einem_knoten_liefert_ursprung_ohne_absturz() {
        let positions = layout_3d(1, &[], &LayoutParams::default());
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0], Vec3::ZERO);
    }

    #[test]
    fn layout_mit_leerem_graphen_liefert_leeren_vektor() {
        let positions = layout_3d(0, &[], &LayoutParams::default());
        assert!(positions.is_empty());
    }

    #[test]
    fn federkraft_zieht_verbundene_knoten_naeher_als_unverbundene() {
        // Das Gleichgewicht liegt wegen der Abstoßungskraft oberhalb von
        // `spring_length` (Federkraft muss die Abstoßung ausgleichen) --
        // die Feder wirkt trotzdem: mit Kante ist der Abstand deutlich
        // kleiner als bei ansonsten identischen, aber unverbundenen
        // Knoten (reine Abstoßung).
        let params = LayoutParams {
            iterations: 500,
            ..Default::default()
        };
        let with_edge = layout_3d(2, &[(0, 1)], &params);
        let without_edge = layout_3d(2, &[], &params);
        let dist_with_edge = with_edge[0].sub(with_edge[1]).length();
        let dist_without_edge = without_edge[0].sub(without_edge[1]).length();
        assert!(
            dist_with_edge < dist_without_edge,
            "verbunden ({dist_with_edge}) sollte näher liegen als unverbunden ({dist_without_edge})"
        );
        // Gleichgewicht (Feder = Abstoßung) liegt für die Default-Parameter
        // rechnerisch bei ca. 72-73, nicht bei spring_length=60 selbst.
        assert!(
            (dist_with_edge - 72.6).abs() < 5.0,
            "Abstand {dist_with_edge} sollte nahe am rechnerischen Gleichgewicht ~72.6 liegen"
        );
    }

    #[test]
    fn normalized_bei_nulllaenge_liefert_nullvektor_statt_nan() {
        assert_eq!(Vec3::ZERO.normalized(), Vec3::ZERO);
    }

    #[test]
    fn layout_produziert_keine_nan_werte() {
        let params = LayoutParams::with_iterations(50);
        let positions = layout_3d(5, &[(0, 1), (1, 2), (2, 3), (3, 4)], &params);
        for p in positions {
            assert!(p.x.is_finite() && p.y.is_finite() && p.z.is_finite());
        }
    }

    #[test]
    fn ungueltiger_kanten_index_wird_uebersprungen_statt_zu_stuerzen() {
        let params = LayoutParams::with_iterations(10);
        let positions = layout_3d(3, &[(0, 99)], &params);
        assert_eq!(positions.len(), 3);
    }
}
