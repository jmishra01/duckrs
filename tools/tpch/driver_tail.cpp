} // namespace tpch
using namespace tpch;
static const char *NAMES[] = {"part", "partsupp", "supplier", "customer", "orders", "lineitem", "", "", "nation", "region"};
int main(int argc, char **argv) {
	double sf = atof(argv[1]);
	std::string out = argv[2];
	static DBGenContext ctx;
	table = (1 << CUST) | (1 << SUPP) | (1 << NATION) | (1 << REGION) | (1 << PART_PSUPP) | (1 << ORDER_LINE);
	force = insert_segments = delete_segments = insert_orders_segment = insert_lineitem_segment = delete_segment = 0;
	verbose = set_seeds = updates = 0;
	d_path = NULL;
	auto tdefs = ctx.tdefs;
	tdefs[PART].base = 200000; tdefs[PSUPP].base = 200000; tdefs[SUPP].base = 10000; tdefs[CUST].base = 150000;
	tdefs[ORDER].base = 150000 * ORDERS_PER_CUST; tdefs[LINE].base = 150000 * ORDERS_PER_CUST;
	tdefs[ORDER_LINE].base = 150000 * ORDERS_PER_CUST; tdefs[PART_PSUPP].base = 200000;
	tdefs[NATION].base = NATIONS_MAX; tdefs[REGION].base = NATIONS_MAX;
	if (sf < MIN_SCALE) {
		int int_scale = (int)(1000 * sf);
		ctx.scale_factor = 1;
		for (int i = PART; i < REGION; i++) {
			tdefs[i].base = (DSS_HUGE)(int_scale * tdefs[i].base) / 1000;
			if (tdefs[i].base < 1) tdefs[i].base = 1;
		}
	} else {
		ctx.scale_factor = (long)sf;
	}
	load_dists(TEXT_POOL_SIZE, &ctx);
	tdefs[NATION].base = nations.count;
	tdefs[REGION].base = regions.count;
	static tpch_append_information info[REGION + 1];
	for (int i = PART; i <= REGION; i++) {
		if (NAMES[i][0]) info[i].f = fopen((out + "/" + NAMES[i] + ".tbl").c_str(), "w");
	}
	int tables[] = {SUPP, CUST, ORDER_LINE, PART_PSUPP, NATION, REGION};
	for (int t : tables) {
		DSS_HUGE count = t < NATION ? tdefs[t].base * ctx.scale_factor : tdefs[t].base;
		for (DSS_HUGE i = 1; i <= count; i++) {
			row_start(t, &ctx);
			switch (t) {
			case ORDER_LINE: GenerateOrderLine(i, info, &ctx); break;
			case SUPP: GenerateSupplier(i, info, &ctx); break;
			case CUST: GenerateCustomer(i, info, &ctx); break;
			case PART_PSUPP: GeneratePartAndPartsupp(i, info, &ctx); break;
			case NATION: GenerateNation(i, info, &ctx); break;
			case REGION: GenerateRegion(i, info, &ctx); break;
			}
			row_stop_h(t, &ctx);
		}
	}
	for (int i = PART; i <= REGION; i++) if (info[i].f) fclose(info[i].f);
	return 0;
}
