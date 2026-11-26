// Ported from duckdb/extension/tpcds/dsdgen/dsdgen-c/w_item.cpp @ rust-port-baseline

//! The `item` table.

use super::Ctx;
use super::columns::*;
use super::constants::*;
use super::decimal::{Decimal, OP_MULT, decimal_t_op, strtodec};
use super::scd::{change_scd, change_scd_ptr};
use super::tables::*;
use crate::append_info::AppendInfo;

/// `struct W_ITEM_TBL`
#[derive(Clone, Default)]
struct ItemTbl {
    i_item_sk: i64,
    i_item_id: Vec<u8>,
    i_rec_start_date_id: i64,
    i_rec_end_date_id: i64,
    i_item_desc: Vec<u8>,
    i_current_price: Decimal,
    i_wholesale_cost: Decimal,
    i_brand_id: i64,
    i_brand: Vec<u8>,
    i_class_id: i64,
    i_class: &'static [u8],
    i_category_id: i64,
    i_category: &'static [u8],
    i_manufact_id: i64,
    i_manufact: Vec<u8>,
    i_size: Option<&'static [u8]>,
    i_formulation: Vec<u8>,
    i_color: &'static [u8],
    i_units: &'static [u8],
    i_container: &'static [u8],
    i_manager_id: i64,
    i_product_name: Vec<u8>,
}

impl ItemTbl {
    /// `memset(r, 0, sizeof(struct W_ITEM_TBL))`, keeping the buffers.
    fn clear(&mut self) {
        let mut buffers = [
            std::mem::take(&mut self.i_item_id),
            std::mem::take(&mut self.i_item_desc),
            std::mem::take(&mut self.i_brand),
            std::mem::take(&mut self.i_manufact),
            std::mem::take(&mut self.i_formulation),
            std::mem::take(&mut self.i_product_name),
        ];
        buffers.iter_mut().for_each(Vec::clear);
        let [item_id, item_desc, brand, manufact, formulation, product_name] = buffers;
        *self = Self {
            i_item_id: item_id,
            i_item_desc: item_desc,
            i_brand: brand,
            i_manufact: manufact,
            i_formulation: formulation,
            i_product_name: product_name,
            ..Self::default()
        };
    }
}

/// `g_w_item`, `g_OldValues` and the statics of `mk_w_item`.
#[derive(Default)]
pub(crate) struct State {
    r: ItemTbl,
    old: ItemTbl,
    d_min_markdown: Decimal,
    d_max_markdown: Decimal,
}

/// `mk_w_item`
pub(crate) fn mk_w_item(ctx: &mut Ctx, info_arr: &mut AppendInfo, index: i64) {
    let mut st = std::mem::take(&mut ctx.item);
    if !ctx.init.mk_w_item_init {
        st.d_min_markdown = strtodec(MIN_ITEM_MARKDOWN_PCT.as_bytes());
        st.d_max_markdown = strtodec(MAX_ITEM_MARKDOWN_PCT.as_bytes());
        ctx.init.mk_w_item_init = true;
    }
    let r = &mut st.r;
    let old = &mut st.old;
    r.clear();
    let mut b_first_record = false;
    ctx.null_set(ITEM, I_NULLS);
    // build the new value
    r.i_item_sk = index;

    let (n_min, n_index) = ctx.pick_distribution_int("i_manager_id", 2, 1, I_MANAGER_ID);
    let n_max = ctx.dist_member_int("i_manager_id", n_index, 3);
    r.i_manager_id = ctx.genrand_key(DIST_UNIFORM, i64::from(n_min), i64::from(n_max), 0, I_MANAGER_ID);

    // if we have generated the required history for this business key and generate a new one
    // then reset associated fields (e.g., rec_start_date minimums)
    if ctx.set_scd_keys(
        I_ITEM_ID,
        index,
        &mut r.i_item_id,
        &mut r.i_rec_start_date_id,
        &mut r.i_rec_end_date_id,
    ) {
        // some fields are not changed, even when a new version of the row is written
        b_first_record = true;
    }

    // select the random number that controls if a field changes from one record to the next
    let mut n_field_change_flags = ctx.next_random(I_SCD) as i32;

    // the rest of the record in a history-keeping dimension can either be a new data value or
    // not; use a random number and its bit pattern to determine which fields to replace and
    // which to retain
    ctx.gen_text(&mut r.i_item_desc, 1, RS_I_ITEM_DESC, I_ITEM_DESC);
    change_scd(
        &mut r.i_item_desc,
        &mut old.i_item_desc,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (sz_min_price, n_index) = ctx.pick_distribution_str("i_current_price", 2, 1, I_CURRENT_PRICE);
    let sz_max_price = ctx.dist_member_str("i_current_price", n_index, 3);
    let d_min_price = strtodec(sz_min_price);
    let d_max_price = strtodec(sz_max_price);
    ctx.genrand_decimal(
        &mut r.i_current_price,
        DIST_UNIFORM,
        &d_min_price,
        &d_max_price,
        None,
        I_CURRENT_PRICE,
    );
    // changeSCD(SCD_INT, ...) of a decimal copies its first int: the flags
    change_scd(
        &mut r.i_current_price.flags,
        &mut old.i_current_price.flags,
        &mut n_field_change_flags,
        b_first_record,
    );

    let mut d_markdown = Decimal::default();
    let (min, max) = (st.d_min_markdown, st.d_max_markdown);
    ctx.genrand_decimal(&mut d_markdown, DIST_UNIFORM, &min, &max, None, I_WHOLESALE_COST);
    decimal_t_op(&mut r.i_wholesale_cost, OP_MULT, &r.i_current_price, &d_markdown);
    change_scd(
        &mut r.i_wholesale_cost,
        &mut old.i_wholesale_cost,
        &mut n_field_change_flags,
        b_first_record,
    );

    let (category_id, category) = ctx.hierarchy_item(I_CATEGORY);
    r.i_category_id = category_id;
    r.i_category = category;
    // the class_id and name are generated together, and the name is derived from the id
    let (class_id, class) = ctx.hierarchy_item(I_CLASS);
    r.i_class_id = class_id;
    r.i_class = class;
    change_scd(
        &mut r.i_class_id,
        &mut old.i_class_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.i_brand_id = ctx.hierarchy_item_brand(&mut r.i_brand, index);
    change_scd(
        &mut r.i_brand_id,
        &mut old.i_brand_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    // some categories have meaningful sizes, some don't
    if r.i_category_id != 0 {
        let b_use_size = ctx.dist_member_int("categories", r.i_category_id as i32, 3);
        r.i_size = Some(ctx.pick_distribution_str("sizes", 1, b_use_size + 2, I_SIZE).0);
        change_scd_ptr(&mut n_field_change_flags);
    } else {
        r.i_size = None;
    }

    let (n_min, n_index) = ctx.pick_distribution_int("i_manufact_id", 2, 1, I_MANUFACT_ID);
    let n_max = ctx.dist_member_int("i_manufact_id", n_index, 3);
    r.i_manufact_id = i64::from(ctx.genrand_integer(DIST_UNIFORM, n_min, n_max, 0, I_MANUFACT_ID));
    change_scd(
        &mut r.i_manufact_id,
        &mut old.i_manufact_id,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.mk_word(
        &mut r.i_manufact,
        "syllables",
        i64::from(r.i_manufact_id as i32),
        RS_I_MANUFACT,
        ITEM,
    );
    change_scd(
        &mut r.i_manufact,
        &mut old.i_manufact,
        &mut n_field_change_flags,
        b_first_record,
    );

    ctx.gen_charset(
        &mut r.i_formulation,
        DIGITS,
        RS_I_FORMULATION,
        RS_I_FORMULATION,
        I_FORMULATION,
    );
    ctx.embed_string(&mut r.i_formulation, "colors", 1, 2, I_FORMULATION);
    change_scd(
        &mut r.i_formulation,
        &mut old.i_formulation,
        &mut n_field_change_flags,
        b_first_record,
    );

    r.i_color = ctx.pick_distribution_str("colors", 1, 2, I_COLOR).0;
    change_scd_ptr(&mut n_field_change_flags);

    r.i_units = ctx.pick_distribution_str("units", 1, 1, I_UNITS).0;
    change_scd_ptr(&mut n_field_change_flags);

    r.i_container = ctx.pick_distribution_str("container", 1, 1, ITEM).0;
    change_scd_ptr(&mut n_field_change_flags);

    ctx.mk_word(
        &mut r.i_product_name,
        "syllables",
        i64::from(index as i32),
        RS_I_PRODUCT_NAME,
        ITEM,
    );

    // i_promo_sk is not a column of item, but its draws are part of the row
    let _i_promo_sk = ctx.mk_join(I_PROMO_SK, PROMOTION, 1);
    let n_temp = ctx.genrand_integer(DIST_UNIFORM, 1, 100, 0, I_PROMO_SK);
    let _i_promo_sk = if n_temp > I_PROMO_PERCENTAGE { -1 } else { _i_promo_sk };

    // if this is the first of a set of revisions, then baseline the old values
    if b_first_record || index == 1 {
        old.clone_from(r);
    }

    let info = info_arr.row_start(ITEM, ctx.tdef(ITEM).k_null_bit_map);
    info.append_key(r.i_item_sk, I_ITEM_SK);
    info.append_varchar(&r.i_item_id, I_ITEM_ID);
    info.append_date(r.i_rec_start_date_id, I_REC_START_DATE_ID);
    info.append_date(r.i_rec_end_date_id, I_REC_END_DATE_ID);
    info.append_varchar(&r.i_item_desc, I_ITEM_DESC);
    info.append_decimal(&r.i_current_price, I_CURRENT_PRICE);
    info.append_decimal(&r.i_wholesale_cost, I_WHOLESALE_COST);
    info.append_key(r.i_brand_id, I_BRAND_ID);
    info.append_varchar(&r.i_brand, I_BRAND);
    info.append_key(r.i_class_id, I_CLASS_ID);
    info.append_varchar(r.i_class, I_CLASS);
    info.append_key(r.i_category_id, I_CATEGORY_ID);
    info.append_varchar(r.i_category, I_CATEGORY);
    info.append_key(r.i_manufact_id, I_MANUFACT_ID);
    info.append_varchar(&r.i_manufact, I_MANUFACT);
    info.append_varchar_opt(r.i_size, I_SIZE);
    info.append_varchar(&r.i_formulation, I_FORMULATION);
    info.append_varchar(r.i_color, I_COLOR);
    info.append_varchar(r.i_units, I_UNITS);
    info.append_varchar(r.i_container, I_CONTAINER);
    info.append_key(r.i_manager_id, I_MANAGER_ID);
    info.append_varchar(&r.i_product_name, I_PRODUCT_NAME);
    info.row_end();

    ctx.item = st;
}
