pub use prometheus::core::Collector;
pub use prometheus::{
    Counter, CounterVec, Error as PrometheusError, Gauge, GaugeVec, Histogram, HistogramOpts,
    HistogramVec, Opts, Registry,
};

use std::collections::HashMap;

pub trait MetricsRegistry: Clone + Send + Sync + 'static + Sized {
    fn new_gauge(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
    ) -> Result<Box<Gauge>, PrometheusError>;

    fn new_gauge_vec(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
        variable_labels: Vec<String>,
    ) -> Result<Box<GaugeVec>, PrometheusError>;

    fn new_counter(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
    ) -> Result<Box<Counter>, PrometheusError>;

    fn new_counter_vec(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
        variable_labels: Vec<String>,
    ) -> Result<Box<CounterVec>, PrometheusError>;

    fn new_histogram(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
        buckets: Vec<f64>,
    ) -> Result<Box<Histogram>, PrometheusError>;

    fn new_histogram_vec(
        &self,
        name: String,
        help: String,
        const_labels: HashMap<String, String>,
        variable_labels: Vec<String>,
        buckets: Vec<f64>,
    ) -> Result<Box<HistogramVec>, PrometheusError>;

    fn unregister(&self, metric: Box<dyn Collector>);
}
