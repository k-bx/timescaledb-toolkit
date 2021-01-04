
use std::{convert::TryInto, hash::{BuildHasher, Hasher}, mem::size_of, slice};

use serde::{Deserialize, Serialize};

use pgx::*;
use pg_sys::{Datum, Oid};

use flat_serialize::*;

use crate::{
    aggregate_utils::{get_collation, in_aggregate_context},
    debug_inout_funcs,
    flatten,
    palloc::Internal,
    pg_type,
    serialization::{ShortTypeId, PgCollationId},
};

use hyperloglog::{HyperLogLog as HLL, HyperLogLogger};

#[derive(Clone, Serialize, Deserialize)]
pub struct HyperLogLogTrans {
    logger: HyperLogLogger<Datum, DatumHashBuilder>,
}

#[allow(non_camel_case_types)]
type int = i32;
type AnyElement = Datum;

#[pg_extern]
pub fn hyperloglog_trans(
    state: Option<Internal<HyperLogLogTrans>>,
    size: int,
    value: Option<AnyElement>,
    fc: pg_sys::FunctionCallInfo,
) -> Option<Internal<HyperLogLogTrans>> {
    unsafe {
        in_aggregate_context(fc, || {
            //TODO is this the right way to handle NULL?
            let value = match value {
                None => return state,
                Some(value) => value,
            };
            let mut state = match state {
                None => {
                    // TODO specialize hash function for bytea types?
                    //      ints? floats? uuids? other primitive types?
                    let size: usize = size.try_into().unwrap();
                    let b = size.checked_next_power_of_two().unwrap().trailing_zeros();
                    let typ = pgx::get_getarg_type(fc, 2);
                    let collation = get_collation(fc);
                    let hasher = DatumHashBuilder::from_type_id(typ, collation);
                    let trans = HyperLogLogTrans {
                        logger: HyperLogLogger::with_hash(b as usize, hasher),
                    };
                    trans.into()
                },
                Some(state) => state,
            };
            state.logger.add(&value);
            Some(state)
        })
    }
}

#[pg_extern]
pub fn hyperloglog_combine(
    state1: Option<Internal<HyperLogLogTrans>>,
    state2: Option<Internal<HyperLogLogTrans>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<Internal<HyperLogLogTrans>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            match (state1, state2) {
                (None, None) => None,
                (None, Some(state2)) => Some(state2.clone().into()),
                (Some(state1), None) => Some(state1.clone().into()),
                (Some(state1), Some(state2)) => {
                    let logger = HLL::merge(
                        &state1.logger.as_hyperloglog(),
                        &state2.logger.as_hyperloglog(),
                    );
                    Some(HyperLogLogTrans{
                        logger,
                    }.into())
                }
            }
        })
    }
}

#[allow(non_camel_case_types)]
type bytea = pg_sys::Datum;

#[pg_extern]
pub fn hyperloglog_serialize(
    state: Internal<HyperLogLogTrans>,
) -> bytea {
    crate::do_serialize!(state)
}

#[pg_extern]
pub fn hyperloglog_deserialize(
    bytes: bytea,
    _internal: Option<Internal<()>>,
) -> Internal<HyperLogLogTrans> {
    crate::do_deserialize!(bytes, HyperLogLogTrans)
}

extension_sql!(r#"
CREATE TYPE Hyperloglog;
"#);

pg_type!{
    #[derive(Debug)]
    struct HyperLogLog {
        // Oids are stored in postgres arrays, so it should be safe to store them
        // in our types as long as we do send/recv and in/out correctly
        // see https://github.com/postgres/postgres/blob/b8d0cda53377515ac61357ec4a60e85ca873f486/src/include/utils/array.h#L90
        element_type: ShortTypeId,
        collation: PgCollationId,
        b: u32,
        registers: [u8; (1 as usize) << self.b],
    }
}

debug_inout_funcs!(HyperLogLog);

#[pg_extern]
fn hyperloglog_final(
    state: Option<Internal<HyperLogLogTrans>>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> Option<HyperLogLog<'static>> {
    unsafe {
        in_aggregate_context(fcinfo, || {
            let state = match state {
                None => return None,
                Some(state) => state,
            };

            let (element_type, collation) =  {
                let hasher = state.logger.buildhasher();
                (ShortTypeId(hasher.type_id), PgCollationId(hasher.collation))
            };
            let log = state.logger.as_hyperloglog();

            // we need to flatten the vector to a single buffer that contains
            // both the size, the data, and the varlen header
            flatten!(
                HyperLogLog {
                    element_type: &element_type,
                    collation: &collation,
                    b: &(log.b as u32),
                    registers: log.registers,
                }
            ).into()
        })
    }
}

extension_sql!(r#"
CREATE OR REPLACE FUNCTION Hyperloglog_in(cstring) RETURNS Hyperloglog IMMUTABLE STRICT PARALLEL SAFE LANGUAGE C AS 'MODULE_PATHNAME', 'hyperloglog_in_wrapper';
CREATE OR REPLACE FUNCTION Hyperloglog_out(Hyperloglog) RETURNS CString IMMUTABLE STRICT PARALLEL SAFE LANGUAGE C AS 'MODULE_PATHNAME', 'hyperloglog_out_wrapper';

CREATE TYPE Hyperloglog (
    INTERNALLENGTH = variable,
    INPUT = Hyperloglog_in,
    OUTPUT = Hyperloglog_out,
    STORAGE = extended
);

CREATE AGGREGATE hyperloglog(size int, value AnyElement)
(
    stype = internal,
    sfunc=hyperloglog_trans,
    finalfunc = hyperloglog_final,
    combinefunc = hyperloglog_combine,
    serialfunc = hyperloglog_serialize,
    deserialfunc = hyperloglog_deserialize
);
"#);


#[pg_extern]
pub fn hyperloglog_count<'input>(
    hyperloglog: HyperLogLog<'input>,
) -> i64 {
    // count does not depend on the type parameters
    HLL::<()> {
        registers: hyperloglog.registers,
        b: *hyperloglog.b as _,
        buildhasher: Default::default(),
        phantom: Default::default(),
    }.count()
}




// TODO move to it's own mod if we reuse it
struct DatumHashBuilder {
    info: pg_sys::FunctionCallInfo,
    type_id: pg_sys::Oid,
    collation: pg_sys::Oid,
}

impl DatumHashBuilder {

    unsafe fn from_type_id(type_id: pg_sys::Oid, collation: Option<Oid>) -> Self {
        let entry = pg_sys::lookup_type_cache(
            type_id,
            pg_sys::TYPECACHE_HASH_EXTENDED_PROC_FINFO as _
        );
        Self::from_type_cache_entry(entry, collation)
    }

    unsafe fn from_type_cache_entry(tentry: *const pg_sys::TypeCacheEntry, collation: Option<Oid>) -> Self {
        let flinfo  = if (*tentry).hash_extended_proc_finfo.fn_addr.is_some() {
            &(*tentry).hash_extended_proc_finfo
        } else {
            pgx::error!("no hash function");
        };

        // 1 argument for the key, 1 argument for the seed
        let size = size_of::<pg_sys::FunctionCallInfoBaseData>()
            + size_of::<pg_sys::NullableDatum>() * 2;
        let mut info = pg_sys::palloc0(size) as pg_sys::FunctionCallInfo;

        (*info).flinfo = flinfo as *const pg_sys::FmgrInfo as *mut pg_sys::FmgrInfo;
        (*info).context = std::ptr::null_mut();
        (*info).resultinfo = std::ptr::null_mut();
        (*info).fncollation = (*tentry).typcollation;
        (*info).isnull = false;
        (*info).nargs = 1;

        let collation = match collation {
            Some(collation) => collation,
            None => (*tentry).typcollation,
        };

        Self { info, type_id: (*tentry).type_id, collation }
    }
}

impl Clone for DatumHashBuilder {
    fn clone(&self) -> Self {
        Self { info: self.info, type_id: self.type_id, collation: self.collation }
    }
}

impl BuildHasher for DatumHashBuilder {
    type Hasher = DatumHashBuilder;

    fn build_hasher(&self) -> Self::Hasher {
        Self { info: self.info, type_id: self.type_id, collation: self.collation }
    }
}

impl Hasher for DatumHashBuilder {
    fn finish(&self) -> u64 {
        //FIXME ehhh, this is wildly unsafe, should at least have a separate hash
        //      buffer for each, probably should have separate args
        let value = unsafe {
            let value = (*(*self.info).flinfo).fn_addr.unwrap()(self.info);
            (*self.info).args.as_mut_slice(1)[0] = pg_sys::NullableDatum {
                value: 0,
                isnull: true,
            };
            (*self.info).isnull = false;
            //FIXME 32bit vs 64 bit get value from datum on 32b arch
            value
        };
        value as u64

    }

    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() != size_of::<usize>() {
            panic!("invalid datum hash")
        }

        let mut b = [0; size_of::<usize>()];
        for i in 0..size_of::<usize>() {
            b[i] = bytes[i]
        }
        self.write_usize(usize::from_ne_bytes(b))
    }

    fn write_usize(&mut self, i: usize) {
        unsafe {
            (*self.info).args.as_mut_slice(1)[0] = pg_sys::NullableDatum {
                value: i,
                isnull: false,
            };
            (*self.info).isnull = false;
        }
    }
}

impl PartialEq for DatumHashBuilder {
    fn eq(&self, other: &Self) -> bool {
        self.type_id.eq(&other.type_id)
    }
}

impl Eq for DatumHashBuilder {}

impl Serialize for DatumHashBuilder {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer {
        let collation =
            if self.collation == 0 {
                None
            } else {
                Some(PgCollationId(self.collation))
            };
        (ShortTypeId(self.type_id), collation).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DatumHashBuilder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de> {

        let (type_id, collation) = <(ShortTypeId, Option<PgCollationId>)>::deserialize(deserializer)?;
        //FIXME no collation?
        let deserialized = unsafe {
            Self::from_type_id(type_id.0, collation.map(|c| c.0))
        };
        Ok(deserialized)
    }
}


#[cfg(any(test, feature = "pg_test"))]
mod tests {
    use pgx::*;

    #[pg_test]
    fn test_hll_aggregate() {
        Spi::execute(|client| {
            let text = client
                .select("SELECT hyperloglog(32, v::float)::TEXT FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<String>();
            assert_eq!(text.unwrap(), "HyperLogLogData { header: 208, version: 1, padding: [0, 0, 0], element_type: ShortTypeId(701), collation: PgCollationId(0), b: 5, registers: [2, 5, 2, 3, 2, 6, 3, 2, 1, 3, 5, 3, 3, 3, 3, 3, 6, 3, 0, 4, 3, 6, 0, 2, 6, 1, 2, 9, 3, 10, 2, 2] }");

            let count = client
                .select("SELECT hyperloglog_count(hyperloglog(32, v::float)) FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<i32>();
            assert_eq!(count, Some(108));
        });
    }

    #[pg_test]
    fn test_hll_aggregate_int() {
        Spi::execute(|client| {
            let text = client
                .select("SELECT hyperloglog(32, v::int)::TEXT FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<String>();
            assert_eq!(text.unwrap(), "HyperLogLogData { header: 208, version: 1, padding: [0, 0, 0], element_type: ShortTypeId(23), collation: PgCollationId(0), b: 5, registers: [6, 2, 6, 3, 7, 2, 5, 3, 2, 3, 4, 4, 0, 2, 2, 3, 1, 4, 2, 2, 2, 2, 3, 2, 2, 5, 1, 3, 3, 3, 3, 3] }");

            let count = client
                .select("SELECT hyperloglog_count(hyperloglog(32, v::int)) FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<i32>();
            assert_eq!(count, Some(113));
        });
    }

    #[pg_test]
    fn test_hll_aggregate_text() {
        Spi::execute(|client| {
            let text = client
                .select("SELECT hyperloglog(32, v::text)::TEXT FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<String>();
            assert_eq!(text.unwrap(), "HyperLogLogData { header: 208, version: 1, padding: [0, 0, 0], element_type: ShortTypeId(25), collation: PgCollationId(100), b: 5, registers: [4, 2, 1, 1, 5, 2, 5, 1, 6, 5, 6, 1, 3, 3, 4, 3, 2, 3, 0, 3, 3, 5, 2, 3, 8, 5, 2, 1, 4, 1, 4, 3] }");

            let count = client
                .select("SELECT hyperloglog_count(hyperloglog(32, v::int)) FROM generate_series(1, 100) v", None, None)
                .first()
                .get_one::<i32>();
            assert_eq!(count, Some(113));
        });
    }

    //TODO test continuous aggregates
}
