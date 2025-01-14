
use pgx::*;

use super::*;

use crate::{
    flatten,
};

// TODO is (immutable, parallel_safe) correct?
#[pg_extern(
    immutable,
    parallel_safe,
    name="sort",
    schema="toolkit_experimental"
)]
pub fn sort_pipeline_element<'p, 'e>(
) -> toolkit_experimental::UnstableTimeseriesPipelineElement<'e> {
    unsafe {
        flatten!(
            UnstableTimeseriesPipelineElement {
                element: Element::Sort {}
            }
        )
    }
}

pub fn sort_timeseries(
    series: &toolkit_experimental::TimeSeries,
) -> toolkit_experimental::TimeSeries<'static> {
    match series.series {
        SeriesType::GappyNormalSeries{..} | SeriesType::NormalSeries{..} | SeriesType::SortedSeries{..} => series.in_current_context(),
        SeriesType::ExplicitSeries{points, ..} => {
            unsafe {
                let mut points = points.to_vec();
                points.sort_by(|a, b| a.ts.cmp(&b.ts));

                flatten!(
                    TimeSeries {
                        series: SeriesType::SortedSeries {
                            num_points: points.len() as u64,
                            points: &points,
                        }
                    }
                )
            }
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use pgx::*;

    #[pg_test]
    fn test_pipeline_sort() {
        Spi::execute(|client| {
            // using the search path trick for this test b/c the operator is
            // difficult to spot otherwise.
            let sp = client.select("SELECT format(' %s, toolkit_experimental',current_setting('search_path'))", None, None).first().get_one::<String>().unwrap();
            client.select(&format!("SET LOCAL search_path TO {}", sp), None, None);
            client.select("SET timescaledb_toolkit_acknowledge_auto_drop TO 'true'", None, None);

            client.select(
                "CREATE TABLE series(time timestamptz, value double precision)",
                None,
                None
            );
            client.select(
                "INSERT INTO series \
                    VALUES \
                    ('2020-01-04 UTC'::TIMESTAMPTZ, 25.0), \
                    ('2020-01-01 UTC'::TIMESTAMPTZ, 10.0), \
                    ('2020-01-03 UTC'::TIMESTAMPTZ, 20.0), \
                    ('2020-01-02 UTC'::TIMESTAMPTZ, 15.0), \
                    ('2020-01-05 UTC'::TIMESTAMPTZ, 30.0)",
                None,
                None
            );

            let val = client.select(
                "SELECT (timeseries(time, value))::TEXT FROM series",
                None,
                None
            )
                .first()
                .get_one::<String>();
            assert_eq!(val.unwrap(), "[\
                {\"ts\":\"2020-01-04 00:00:00+00\",\"val\":25.0},\
                {\"ts\":\"2020-01-01 00:00:00+00\",\"val\":10.0},\
                {\"ts\":\"2020-01-03 00:00:00+00\",\"val\":20.0},\
                {\"ts\":\"2020-01-02 00:00:00+00\",\"val\":15.0},\
                {\"ts\":\"2020-01-05 00:00:00+00\",\"val\":30.0}\
            ]");


            let val = client.select(
                "SELECT (timeseries(time, value) |> sort())::TEXT FROM series",
                None,
                None
            )
                .first()
                .get_one::<String>();
            assert_eq!(val.unwrap(), "[\
                {\"ts\":\"2020-01-01 00:00:00+00\",\"val\":10.0},\
                {\"ts\":\"2020-01-02 00:00:00+00\",\"val\":15.0},\
                {\"ts\":\"2020-01-03 00:00:00+00\",\"val\":20.0},\
                {\"ts\":\"2020-01-04 00:00:00+00\",\"val\":25.0},\
                {\"ts\":\"2020-01-05 00:00:00+00\",\"val\":30.0}\
            ]");
        });
    }
}