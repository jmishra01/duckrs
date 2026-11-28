// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_timetbl.cpp @ rust-port-baseline

//! The `time_dim` table: one row per second of the day.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::tables::TIME;
use crate::append_info::AppendInfo;

/// `mk_w_time`
pub(crate) fn mk_w_time(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    ctx.null_set(TIME, T_NULLS);
    let t_time_sk = index - 1;
    let mut t_time_id = Vec::with_capacity(16);
    mk_bkey(&mut t_time_id, index, T_TIME_ID);
    let t_time = (index - 1) as i32;
    let mut n_temp = (index - 1) as i32;
    let t_second = n_temp % 60;
    n_temp /= 60;
    let t_minute = n_temp % 60;
    n_temp /= 60;
    let t_hour = n_temp % 24;
    let t_am_pm = ctx.dist_member_str("hours", t_hour + 1, 2);
    let t_shift = ctx.dist_member_str("hours", t_hour + 1, 3);
    let t_sub_shift = ctx.dist_member_str("hours", t_hour + 1, 4);
    let t_meal_time = ctx.dist_member_str("hours", t_hour + 1, 5);

    let info = info_arr.row_start(TIME, ctx.tdef(TIME).k_null_bit_map);
    info.append_key(t_time_sk, T_TIME_SK);
    info.append_varchar(&t_time_id, T_TIME_ID);
    info.append_integer(t_time, T_TIME);
    info.append_integer(t_hour, T_HOUR);
    info.append_integer(t_minute, T_MINUTE);
    info.append_integer(t_second, T_SECOND);
    info.append_varchar(t_am_pm, T_AM_PM);
    info.append_varchar(t_shift, T_SHIFT);
    info.append_varchar(t_sub_shift, T_SUB_SHIFT);
    info.append_varchar(t_meal_time, T_MEAL_TIME);
    info.row_end();
}
