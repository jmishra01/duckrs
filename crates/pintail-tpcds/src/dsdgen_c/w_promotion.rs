// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_promotion.cpp @ rust-port-baseline

//! The `promotion` table.

use super::Ctx;
use super::build_support::mk_bkey;
use super::columns::*;
use super::constants::*;
use super::date::{DateT, strtodt};
use super::decimal::{Decimal, strtodec};
use super::tables::*;
use crate::append_info::AppendInfo;

/// `struct W_PROMOTION_TBL`
#[derive(Default)]
struct PromotionTbl {
    p_promo_sk: i64,
    p_promo_id: Vec<u8>,
    p_start_date_id: i64,
    p_end_date_id: i64,
    p_item_sk: i64,
    p_cost: Decimal,
    p_response_target: i32,
    p_promo_name: Vec<u8>,
    p_channel_dmail: bool,
    p_channel_email: bool,
    p_channel_catalog: bool,
    p_channel_tv: bool,
    p_channel_radio: bool,
    p_channel_press: bool,
    p_channel_event: bool,
    p_channel_demo: bool,
    p_channel_details: Vec<u8>,
    p_purpose: &'static [u8],
    p_discount_active: bool,
}

/// `g_w_promotion` and the statics of `mk_w_promotion`.
#[derive(Default)]
pub(crate) struct State {
    r: PromotionTbl,
    start_date: DateT,
}

fn yn(value: bool) -> &'static [u8] {
    if value { b"Y" } else { b"N" }
}

/// `mk_w_promotion`
pub(crate) fn mk_w_promotion(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.promotion);
    if !ctx.init.mk_w_promotion_init {
        st.r = PromotionTbl::default();
        ctx.init.mk_w_promotion_init = true;
        st.start_date = strtodt(DATE_MINIMUM);
    }
    let r = &mut st.r;
    ctx.null_set(PROMOTION, P_NULLS);
    r.p_promo_sk = index;
    mk_bkey(&mut r.p_promo_id, index, P_PROMO_ID);
    r.p_start_date_id = i64::from(
        st.start_date.julian
            + ctx.genrand_integer(
                DIST_UNIFORM,
                PROMO_START_MIN,
                PROMO_START_MAX,
                PROMO_START_MEAN,
                P_START_DATE_ID,
            ),
    );
    r.p_end_date_id = r.p_start_date_id
        + i64::from(ctx.genrand_integer(
            DIST_UNIFORM,
            PROMO_LEN_MIN,
            PROMO_LEN_MAX,
            PROMO_LEN_MEAN,
            P_END_DATE_ID,
        ));
    r.p_item_sk = ctx.mk_join(P_ITEM_SK, ITEM, 1);
    r.p_cost = strtodec(b"1000.00");
    r.p_response_target = 1;
    ctx.mk_word(
        &mut r.p_promo_name,
        "syllables",
        i64::from(index as i32),
        PROMO_NAME_LEN,
        P_PROMO_NAME,
    );
    // each flag is tested after one more shift left, so only the first one can be set
    let mut n_flags = ctx.genrand_integer(DIST_UNIFORM, 0, 511, 0, P_CHANNEL_DMAIL);
    r.p_channel_dmail = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_email = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_catalog = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_tv = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_radio = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_press = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_event = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_channel_demo = n_flags & 0x01 != 0;
    n_flags <<= 1;
    r.p_discount_active = n_flags & 0x01 != 0;
    ctx.gen_text(
        &mut r.p_channel_details,
        PROMO_DETAIL_LEN_MIN,
        PROMO_DETAIL_LEN_MAX,
        P_CHANNEL_DETAILS,
    );
    r.p_purpose = ctx.pick_distribution_str("promo_purpose", 1, 1, P_PURPOSE).0;

    let info = info_arr.row_start(PROMOTION, ctx.tdef(PROMOTION).k_null_bit_map);
    info.append_key(r.p_promo_sk, P_PROMO_SK);
    info.append_varchar(&r.p_promo_id, P_PROMO_ID);
    info.append_key(r.p_start_date_id, P_START_DATE_ID);
    info.append_key(r.p_end_date_id, P_END_DATE_ID);
    info.append_key(r.p_item_sk, P_ITEM_SK);
    info.append_decimal(&r.p_cost, P_COST);
    info.append_integer(r.p_response_target, P_RESPONSE_TARGET);
    info.append_varchar(&r.p_promo_name, P_PROMO_NAME);
    info.append_varchar(yn(r.p_channel_dmail), P_CHANNEL_DMAIL);
    info.append_varchar(yn(r.p_channel_email), P_CHANNEL_EMAIL);
    info.append_varchar(yn(r.p_channel_catalog), P_CHANNEL_CATALOG);
    info.append_varchar(yn(r.p_channel_tv), P_CHANNEL_TV);
    info.append_varchar(yn(r.p_channel_radio), P_CHANNEL_RADIO);
    info.append_varchar(yn(r.p_channel_press), P_CHANNEL_PRESS);
    info.append_varchar(yn(r.p_channel_event), P_CHANNEL_EVENT);
    info.append_varchar(yn(r.p_channel_demo), P_CHANNEL_DEMO);
    info.append_varchar(&r.p_channel_details, P_CHANNEL_DETAILS);
    info.append_varchar(r.p_purpose, P_PURPOSE);
    info.append_varchar(yn(r.p_discount_active), P_DISCOUNT_ACTIVE);
    info.row_end();

    ctx.promotion = st;
}
