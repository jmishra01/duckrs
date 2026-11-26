// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_customer_demographics.cpp @ rust-port-baseline

//! The `customer_demographics` table: every combination of the demographic values, in order.

use super::Ctx;
use super::columns::*;
use super::constants::{CD_MAX_CHILDREN, CD_MAX_COLLEGE, CD_MAX_EMPLOYED};
use super::tables::CUSTOMER_DEMOGRAPHICS;
use crate::append_info::AppendInfo;

/// `mk_w_customer_demographics`
pub(crate) fn mk_w_customer_demographics(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    ctx.null_set(CUSTOMER_DEMOGRAPHICS, CD_NULLS);
    let cd_demo_sk = index;
    let mut k_temp = cd_demo_sk - 1;
    let cd_gender = ctx.bitmap_to_dist_str("gender", &mut k_temp, 1);
    let cd_marital_status = ctx.bitmap_to_dist_str("marital_status", &mut k_temp, 1);
    let cd_education_status = ctx.bitmap_to_dist_str("education", &mut k_temp, 1);
    let cd_purchase_estimate = ctx.bitmap_to_dist_int("purchase_band", &mut k_temp, 1);
    let cd_credit_rating = ctx.bitmap_to_dist_str("credit_rating", &mut k_temp, 1);
    let cd_dep_count = (k_temp % CD_MAX_CHILDREN) as i32;
    k_temp /= CD_MAX_CHILDREN;
    let cd_dep_employed_count = (k_temp % CD_MAX_EMPLOYED) as i32;
    k_temp /= CD_MAX_EMPLOYED;
    let cd_dep_college_count = (k_temp % CD_MAX_COLLEGE) as i32;

    let info = info_arr.row_start(CUSTOMER_DEMOGRAPHICS, ctx.tdef(CUSTOMER_DEMOGRAPHICS).k_null_bit_map);
    info.append_key(cd_demo_sk, CD_DEMO_SK);
    info.append_varchar(cd_gender, CD_GENDER);
    info.append_varchar(cd_marital_status, CD_MARITAL_STATUS);
    info.append_varchar(cd_education_status, CD_EDUCATION_STATUS);
    info.append_integer(cd_purchase_estimate, CD_PURCHASE_ESTIMATE);
    info.append_varchar(cd_credit_rating, CD_CREDIT_RATING);
    info.append_integer(cd_dep_count, CD_DEP_COUNT);
    info.append_integer(cd_dep_employed_count, CD_DEP_EMPLOYED_COUNT);
    info.append_integer(cd_dep_college_count, CD_DEP_COLLEGE_COUNT);
    info.row_end();
}
