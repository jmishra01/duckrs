// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_datetbl.cpp @ rust-port-baseline

//! The `date_dim` table: one row per day from 1900-01-01.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::*;
use super::date::*;
use super::tables::DATET;
use crate::append_info::AppendInfo;

/// `struct W_DATE_TBL`
#[derive(Default)]
struct DateTbl {
    d_date_sk: i64,
    d_date_id: Vec<u8>,
    d_month_seq: i32,
    d_week_seq: i32,
    d_quarter_seq: i32,
    d_year: i32,
    d_dow: i32,
    d_moy: i32,
    d_dom: i32,
    d_qoy: i32,
    d_fy_year: i32,
    d_fy_quarter_seq: i32,
    d_fy_week_seq: i32,
    d_day_name: &'static [u8],
    d_holiday: i32,
    d_weekend: bool,
    d_following_holiday: i32,
    d_first_dom: i32,
    d_last_dom: i32,
    d_same_day_ly: i32,
    d_same_day_lq: i32,
    d_current_day: bool,
    d_current_week: bool,
    d_current_month: bool,
    d_current_quarter: bool,
    d_current_year: bool,
}

/// `g_w_date` and the statics of `mk_w_date`.
#[derive(Default)]
pub(crate) struct State {
    r: DateTbl,
    base_date: DateT,
    quarter_name: Vec<u8>,
}

fn yn(value: bool) -> &'static [u8] {
    if value { b"Y" } else { b"N" }
}

/// `mk_w_date`
pub(crate) fn mk_w_date(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.date);
    if !ctx.init.mk_w_date_init {
        let r = &mut st.r;
        r.d_month_seq = 0;
        r.d_week_seq = 1;
        r.d_quarter_seq = 1;
        r.d_current_month = false;
        r.d_current_quarter = false;
        r.d_current_week = false;
        st.base_date = strtodt("1900-01-01");
        ctx.init.mk_w_date_init = true;
    }
    let r = &mut st.r;
    ctx.null_set(DATET, D_NULLS);
    let n_temp = (index + i64::from(st.base_date.julian)) as i32;
    r.d_date_sk = i64::from(n_temp);
    mk_bkey(&mut r.d_date_id, i64::from(n_temp), D_DATE_ID);
    let temp_date = jtodt(n_temp);
    r.d_year = temp_date.year;
    r.d_dow = set_dow(&temp_date);
    r.d_moy = temp_date.month;
    r.d_dom = temp_date.day;
    // set the sequence counts; assumes that the date table starts on a year boundary
    r.d_week_seq = (index as i32 + 6) / 7;
    r.d_month_seq = (r.d_year - 1900) * 12 + r.d_moy - 1;
    r.d_quarter_seq = (r.d_year - 1900) * 4 + r.d_moy / 3 + 1;
    let day_index = day_number(&temp_date);
    r.d_qoy = ctx.dist_member_int("calendar", day_index, 6);
    // fiscal year is identical to calendar year
    r.d_fy_year = r.d_year;
    r.d_fy_quarter_seq = r.d_quarter_seq;
    r.d_fy_week_seq = r.d_week_seq;
    r.d_day_name = WEEKDAY_NAMES[(r.d_dow + 1) as usize];
    r.d_holiday = ctx.dist_member_int("calendar", day_index, 8);
    r.d_weekend = r.d_dow == 5 || r.d_dow == 6;
    r.d_following_holiday = if day_index == 1 {
        // the day before the first of the year is the last day of the year before
        ctx.dist_member_int("calendar", 365 + is_leap(r.d_year - 1), 8)
    } else {
        ctx.dist_member_int("calendar", day_index - 1, 8)
    };
    r.d_first_dom = date_t_op(OP_FIRST_DOM, &temp_date).julian;
    r.d_last_dom = date_t_op(OP_LAST_DOM, &temp_date).julian;
    r.d_same_day_ly = date_t_op(OP_SAME_LY, &temp_date).julian;
    r.d_same_day_lq = date_t_op(OP_SAME_LQ, &temp_date).julian;
    r.d_current_day = r.d_date_sk == i64::from(CURRENT_DAY);
    r.d_current_year = r.d_year == CURRENT_YEAR;
    if r.d_current_year {
        r.d_current_month = r.d_moy == CURRENT_MONTH;
        r.d_current_quarter = r.d_qoy == CURRENT_QUARTER;
        r.d_current_week = r.d_week_seq == CURRENT_WEEK;
    }

    let quarter_name = &mut st.quarter_name;
    quarter_name.clear();
    quarter_name.extend_from_slice(format!("{:4}Q{}", r.d_year, r.d_qoy).as_bytes());
    let info = info_arr.row_start(DATET, ctx.tdef(DATET).k_null_bit_map);
    info.append_key(r.d_date_sk, D_DATE_SK);
    info.append_varchar(&r.d_date_id, D_DATE_ID);
    info.append_date(r.d_date_sk, D_DATE);
    info.append_integer(r.d_month_seq, D_MONTH_SEQ);
    info.append_integer(r.d_week_seq, D_WEEK_SEQ);
    info.append_integer(r.d_quarter_seq, D_QUARTER_SEQ);
    info.append_integer(r.d_year, D_YEAR);
    info.append_integer(r.d_dow, D_DOW);
    info.append_integer(r.d_moy, D_MOY);
    info.append_integer(r.d_dom, D_DOM);
    info.append_integer(r.d_qoy, D_QOY);
    info.append_integer(r.d_fy_year, D_FY_YEAR);
    info.append_integer(r.d_fy_quarter_seq, D_FY_QUARTER_SEQ);
    info.append_integer(r.d_fy_week_seq, D_FY_WEEK_SEQ);
    info.append_varchar(r.d_day_name, D_DAY_NAME);
    info.append_varchar(quarter_name, D_QUARTER_NAME);
    info.append_varchar(yn(r.d_holiday != 0), D_HOLIDAY);
    info.append_varchar(yn(r.d_weekend), D_WEEKEND);
    info.append_varchar(yn(r.d_following_holiday != 0), D_FOLLOWING_HOLIDAY);
    info.append_integer(r.d_first_dom, D_FIRST_DOM);
    info.append_integer(r.d_last_dom, D_LAST_DOM);
    info.append_integer(r.d_same_day_ly, D_SAME_DAY_LY);
    info.append_integer(r.d_same_day_lq, D_SAME_DAY_LQ);
    info.append_varchar(yn(r.d_current_day), D_CURRENT_DAY);
    info.append_varchar(yn(r.d_current_week), D_CURRENT_WEEK);
    info.append_varchar(yn(r.d_current_month), D_CURRENT_MONTH);
    info.append_varchar(yn(r.d_current_quarter), D_CURRENT_QUARTER);
    info.append_varchar(yn(r.d_current_year), D_CURRENT_YEAR);
    info.row_end();

    ctx.date = st;
}
