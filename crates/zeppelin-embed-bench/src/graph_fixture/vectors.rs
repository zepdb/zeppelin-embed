use super::*;
/// Each named stream is independently initialized by the existing tooling helper.
pub type WordStream = Box<dyn FnMut() -> u32>;
pub struct VectorGenerator {
    centroids: Vec<Vec<f64>>,
    noise: WordStream,
    query: WordStream,
    correction: WordStream,
}
impl VectorGenerator {
    pub fn new(factory: &mut dyn FnMut(&str) -> WordStream) -> Result<Self, Error> {
        let mut centroid_words = factory("graph-fixture-v1/centroids");
        let mut centroids = Vec::with_capacity(CENTROIDS);
        for _ in 0..CENTROIDS {
            centroids.push(draw_unit(&mut centroid_words)?);
        }
        Ok(Self {
            centroids,
            noise: factory("graph-fixture-v1/chunk-noise"),
            query: factory("graph-fixture-v1/query-noise"),
            correction: factory("graph-fixture-v1/state-b-vector-noise"),
        })
    }
    pub fn chunk(&mut self, topic: u64, corrected: bool) -> Result<Vec<u32>, Error> {
        let noise = if corrected {
            &mut self.correction
        } else {
            &mut self.noise
        };
        let noise = draw_unit(noise)?;
        let centroid = self
            .centroids
            .get(topic as usize % CENTROIDS)
            .ok_or("centroid outside fixture")?;
        mix(centroid, &noise, 7.0 / 8.0, 1.0 / 8.0)
    }
    pub fn query(&mut self, topic: u64) -> Result<Vec<u32>, Error> {
        let noise = draw_unit(&mut self.query)?;
        let centroid = self
            .centroids
            .get(topic as usize % CENTROIDS)
            .ok_or("centroid outside fixture")?;
        mix(centroid, &noise, 31.0 / 32.0, 1.0 / 32.0)
    }
}
fn unit(values: &mut [f64]) -> Result<(), Error> {
    let mut squared = 0.0;
    for v in values.iter() {
        squared += v * v;
    }
    if squared == 0.0 {
        return Err("all-zero normalization input".into());
    }
    let norm = squared.sqrt();
    for v in values {
        *v /= norm;
    }
    Ok(())
}
fn draw_unit(words: &mut WordStream) -> Result<Vec<f64>, Error> {
    let mut values = (0..DIMS)
        .map(|_| f64::from((words() & 2047) as i32 - 1024))
        .collect::<Vec<_>>();
    unit(&mut values)?;
    Ok(values)
}
fn mix(centroid: &[f64], noise: &[f64], a: f64, b: f64) -> Result<Vec<u32>, Error> {
    let mut mixed = centroid
        .iter()
        .zip(noise)
        .map(|(c, n)| a * c + b * n)
        .collect::<Vec<_>>();
    unit(&mut mixed)?;
    Ok(mixed.into_iter().map(|v| (v as f32).to_bits()).collect())
}
