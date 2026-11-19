//! Generates `fixtures/schema.json`: a synthetic schema with the shape of a large production
//! database (about 260 tables, 250 indexes, 520 statements) that exercises every DDL feature the
//! parser, printer and SQL catalog care about. Names are neutral; nothing comes from a real schema.
//!
//! `cargo run -p spanbolt-ddl --example gen_schema > fixtures/schema.json`
//!
//! Every statement is applied to a `Schema` and the output is its `to_ddl()`, so `ddl` is the
//! canonical GetDatabaseDdl form by construction. Statements the emulator accepts but does not
//! print back (roles and grants) go to `access_ddl`, applied after `ddl`.
//!
//! Output is deterministic: a fixed-seed xorshift drives every choice.

use spanbolt_ddl::{Schema, parse_statement};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

const SCHEMAS: &[&str] = &["sales", "inventory", "audit", "ops", "ops_staging"];

const NOUNS: &[&str] = &[
    "shipment",
    "invoice",
    "supplier",
    "warehouse",
    "carrier",
    "parcel",
    "refund",
    "voucher",
    "coupon",
    "basket",
    "receipt",
    "ledger",
    "payout",
    "transfer",
    "budget",
    "forecast",
    "quota",
    "contract",
    "license",
    "asset",
    "device",
    "sensor",
    "reading",
    "route",
    "depot",
    "vehicle",
    "driver",
    "booking",
    "venue",
    "ticket",
    "seat",
    "survey",
    "response",
    "campaign",
    "segment",
    "audience",
    "message",
    "template",
    "webhook",
    "export",
    "import",
    "batch",
    "job",
    "worker",
    "schedule",
    "calendar",
    "holiday",
    "region",
    "district",
    "station",
    "meter",
    "tariff",
    "subscription",
    "plan",
    "feature",
    "release",
    "build",
    "artifact",
    "review",
    "comment",
    "attachment",
    "folder",
    "label",
    "tag",
    "note",
    "reminder",
    "contact",
    "address",
    "phone",
    "language",
    "currency",
    "rate",
    "measure",
    "unit",
    "recipe",
    "ingredient",
    "menu",
    "course",
];

const QUALIFIERS: &[&str] = &[
    "events",
    "history",
    "snapshots",
    "summary",
    "facts",
    "stats",
    "settings",
    "links",
    "states",
    "metrics",
    "queue",
    "log",
    "items",
    "lines",
    "versions",
    "rules",
];

const COLUMN_WORDS: &[&str] = &[
    "title",
    "label",
    "code",
    "status",
    "phase",
    "category",
    "source",
    "channel",
    "priority",
    "score",
    "weight",
    "volume",
    "price",
    "cost",
    "amount",
    "balance",
    "quantity",
    "duration",
    "latitude",
    "longitude",
    "country",
    "city",
    "postcode",
    "street",
    "notes",
    "summary",
    "description",
    "payload",
    "metadata",
    "attributes",
    "flags",
    "owner_id",
    "parent_ref",
    "external_ref",
    "checksum",
    "version_no",
    "attempts",
    "retries",
    "is_active",
    "is_archived",
    "is_public",
    "is_verified",
    "opened_on",
    "closed_on",
    "due_on",
    "started_at",
    "finished_at",
    "processed_at",
    "expires_at",
    "reviewed_at",
    "tags",
    "aliases",
    "keywords",
    "scores",
    "regions",
    "ratio",
    "rank",
    "level",
    "tier",
    "kind",
    "variant",
    "locale",
    "timezone",
    "digest",
    "signature",
    "config",
    "options",
    "reason",
    "outcome",
];

/// (type, weight). Weights roughly follow the production mix: strings dominate, JSON and arrays
/// are common, BYTES and NUMERIC are rare.
const TYPES: &[(&str, u32)] = &[
    ("STRING(MAX)", 22),
    ("STRING(64)", 8),
    ("STRING(255)", 6),
    ("STRING(36)", 4),
    ("INT64", 12),
    ("FLOAT64", 7),
    ("BOOL", 7),
    ("TIMESTAMP", 9),
    ("DATE", 3),
    ("JSON", 8),
    ("NUMERIC", 2),
    ("BYTES(MAX)", 1),
    ("ARRAY<STRING(MAX)>", 6),
    ("ARRAY<INT64>", 2),
    ("ARRAY<FLOAT64>", 1),
];

const REGIONS: &[&str] = &[
    "north", "south", "east", "west", "central", "global", "legacy",
];

fn pick_type(rng: &mut Rng) -> &'static str {
    let total: u32 = TYPES.iter().map(|t| t.1).sum();
    let mut n = rng.below(total as usize) as u32;
    for (ty, w) in TYPES {
        if n < *w {
            return ty;
        }
        n -= w;
    }
    unreachable!()
}

fn qualify(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// Bare name with the schema replaced by a prefix, for index and constraint names.
fn flat(table: &str) -> String {
    table.replace('.', "_")
}

struct Gen {
    rng: Rng,
    ddl: Vec<String>,
    access: Vec<String>,
}

#[derive(Clone)]
struct Col {
    name: String,
    ty: &'static str,
}

impl Gen {
    fn push(&mut self, s: impl Into<String>) {
        self.ddl.push(s.into());
    }

    fn core(&mut self) {
        for s in SCHEMAS {
            self.push(format!("CREATE SCHEMA {s}"));
        }
        self.push(
            "CREATE SEQUENCE order_number_seq OPTIONS (sequence_kind = 'bit_reversed_positive')",
        );
        self.push(
            "CREATE SEQUENCE ticket_number_seq OPTIONS (sequence_kind = 'bit_reversed_positive', \
             skip_range_min = 1, skip_range_max = 1000, start_with_counter = 10)",
        );
        self.push(
            "CREATE TABLE schema_migrations (
  id STRING(255) NOT NULL,
  author STRING(255) NOT NULL,
  filename STRING(255) NOT NULL,
  applied_at TIMESTAMP NOT NULL,
  applied_order INT64 NOT NULL,
  checksum STRING(64),
  description STRING(255),
) PRIMARY KEY(id, author, filename)",
        );
        self.push(
            "CREATE TABLE schema_migration_lock (
  id INT64 NOT NULL,
  locked BOOL NOT NULL,
  locked_at TIMESTAMP,
  locked_by STRING(255),
) PRIMARY KEY(id)",
        );
        self.push(
            "CREATE TABLE accounts (
  account_id STRING(36) NOT NULL,
  name STRING(MAX) NOT NULL,
  plan STRING(32) NOT NULL DEFAULT ('free'),
  seats INT64 NOT NULL DEFAULT (1),
  settings JSON,
  created_at TIMESTAMP NOT NULL DEFAULT (CURRENT_TIMESTAMP()),
  updated_at TIMESTAMP OPTIONS (allow_commit_timestamp = true),
  CONSTRAINT ck_accounts_seats_positive CHECK(seats > 0),
) PRIMARY KEY(account_id)",
        );
        self.push("CREATE UNIQUE INDEX idx_accounts_name ON accounts(name)");
        self.push(
            "CREATE TABLE account_members (
  account_id STRING(36) NOT NULL,
  member_id STRING(36) NOT NULL,
  email STRING(320),
  login_id STRING(128),
  role STRING(32) NOT NULL DEFAULT ('viewer'),
  member_kind STRING(16) NOT NULL DEFAULT ('person'),
  joined_at TIMESTAMP NOT NULL DEFAULT (CURRENT_TIMESTAMP()),
  CONSTRAINT ck_account_members_person_has_login CHECK(member_kind != 'person' OR login_id IS NOT NULL),
) PRIMARY KEY(account_id, member_id),
  INTERLEAVE IN PARENT accounts ON DELETE CASCADE",
        );
        self.push("CREATE UNIQUE NULL_FILTERED INDEX idx_account_members_login ON account_members(login_id)");
        self.push(
            "CREATE INDEX idx_account_members_email ON account_members(account_id, email), INTERLEAVE IN accounts",
        );
        self.push(
            "CREATE TABLE member_sessions (
  account_id STRING(36) NOT NULL,
  member_id STRING(36) NOT NULL,
  session_id STRING(64) NOT NULL,
  user_agent STRING(MAX),
  last_seen_at TIMESTAMP NOT NULL,
) PRIMARY KEY(account_id, member_id, session_id),
  INTERLEAVE IN PARENT account_members ON DELETE CASCADE, ROW DELETION POLICY (OLDER_THAN(last_seen_at, INTERVAL 30 DAY))",
        );
        self.push(
            "CREATE TABLE catalog_items (
  id STRING(64) NOT NULL,
  account_id STRING(36) NOT NULL,
  name STRING(MAX) NOT NULL,
  sku STRING(64),
  details JSON,
  attributes JSON,
  price NUMERIC,
  tags ARRAY<STRING(MAX)>,
  created_at TIMESTAMP NOT NULL DEFAULT (CURRENT_TIMESTAMP()),
  updated_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp = true),
  name_lower STRING(MAX) AS (LOWER(name)) STORED,
  name_tokens TOKENLIST AS (TOKENIZE_FULLTEXT(name)) HIDDEN,
  name_ngram_tokens TOKENLIST AS (TOKENIZE_NGRAMS(name, ngram_size_max => 3, ngram_size_min => 3)) HIDDEN,
  tags_tokens TOKENLIST AS (TOKEN(tags)) HIDDEN,
  CONSTRAINT fk_catalog_items_account FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE,
) PRIMARY KEY(id)",
        );
        self.push("CREATE INDEX idx_catalog_items_account ON catalog_items(account_id, created_at DESC) STORING (name, sku)");
        self.push("CREATE UNIQUE NULL_FILTERED INDEX idx_catalog_items_sku ON catalog_items(account_id, sku)");
        self.push("CREATE SEARCH INDEX catalog_items_search ON catalog_items(name_tokens, tags_tokens) STORING (account_id) PARTITION BY account_id");
        self.push(
            "CREATE SEARCH INDEX catalog_items_ngram_search ON catalog_items(name_ngram_tokens)",
        );
        self.push(
            "CREATE TABLE item_revisions (
  account_id STRING(36) NOT NULL,
  item_id STRING(64) NOT NULL,
  created_at TIMESTAMP NOT NULL,
  updated_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp = true),
  updated_by STRING(64) NOT NULL,
  latest_revision_id STRING(64) NOT NULL,
  details JSON,
  attributes JSON,
  revision_ids ARRAY<STRING(MAX)> NOT NULL,
  revision_ids_tokens TOKENLIST AS (TOKEN(revision_ids)) HIDDEN,
  CONSTRAINT fk_item_revisions_item FOREIGN KEY(item_id) REFERENCES catalog_items(id) ON DELETE CASCADE,
) PRIMARY KEY(account_id, item_id),
  INTERLEAVE IN PARENT accounts ON DELETE CASCADE",
        );
        self.push(
            "CREATE SEARCH INDEX idx_item_revisions_ids ON item_revisions(revision_ids_tokens)",
        );
        self.push(
            "CREATE TABLE sales.orders (
  id STRING(64) NOT NULL,
  order_number INT64 NOT NULL DEFAULT (GET_NEXT_SEQUENCE_VALUE(SEQUENCE order_number_seq)),
  account_id STRING(36) NOT NULL,
  item_id STRING(64) NOT NULL,
  item_kind STRING(32) NOT NULL,
  status STRING(32) NOT NULL,
  change_kind STRING(32),
  quantity INT64 NOT NULL DEFAULT (1),
  unit_price NUMERIC,
  total NUMERIC AS (unit_price * quantity) STORED,
  placed_at TIMESTAMP NOT NULL,
  notified_by_email_at TIMESTAMP,
  updated_at TIMESTAMP OPTIONS (allow_commit_timestamp = true),
  effective_at TIMESTAMP NOT NULL AS (COALESCE(notified_by_email_at, placed_at)) STORED,
  notes JSON,
  CONSTRAINT fk_sales_orders_account FOREIGN KEY(account_id) REFERENCES accounts(account_id) ON DELETE CASCADE,
  CONSTRAINT fk_sales_orders_item FOREIGN KEY(item_id) REFERENCES catalog_items(id),
  CONSTRAINT ck_sales_orders_quantity CHECK(quantity > 0),
) PRIMARY KEY(id)",
        );
        self.push("CREATE INDEX sales.idx_orders_account_item_placed_at ON sales.orders(account_id, item_id, placed_at DESC) STORING (status, item_kind)");
        self.push("CREATE NULL_FILTERED INDEX sales.idx_orders_notified ON sales.orders(notified_by_email_at)");
        self.push("CREATE UNIQUE INDEX sales.idx_orders_number ON sales.orders(order_number)");
        self.push(
            "CREATE TABLE sales.order_comments (
  id STRING(64) NOT NULL,
  order_id STRING(64) NOT NULL,
  author_id STRING(36) NOT NULL,
  body STRING(MAX) NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT (CURRENT_TIMESTAMP()),
  CONSTRAINT fk_sales_order_comments_order_cascade FOREIGN KEY(order_id) REFERENCES sales.orders(id) ON DELETE CASCADE,
) PRIMARY KEY(id)",
        );
        self.push("CREATE INDEX sales.idx_order_comments_order ON sales.order_comments(order_id, created_at)");
        self.push(
            "CREATE TABLE ops.tickets (
  ticket_number INT64 NOT NULL DEFAULT (GET_NEXT_SEQUENCE_VALUE(SEQUENCE ticket_number_seq)),
  account_id STRING(36) NOT NULL,
  subject STRING(MAX),
  opened_at TIMESTAMP NOT NULL,
  closed_at TIMESTAMP,
  CONSTRAINT ck_ops_tickets_closed_after_opened CHECK(closed_at IS NULL OR closed_at >= opened_at),
) PRIMARY KEY(ticket_number)",
        );
        self.push("CREATE INDEX ops.idx_tickets_open ON ops.tickets(account_id, closed_at) WHERE account_id IS NOT NULL");
        self.push(
            "CREATE TABLE item_links (
  id STRING(64) NOT NULL,
  from_item_id STRING(64) NOT NULL,
  to_item_id STRING(64) NOT NULL,
  link_kind STRING(32) NOT NULL,
  CONSTRAINT fk_item_links_from FOREIGN KEY(from_item_id) REFERENCES catalog_items(id),
  CONSTRAINT fk_item_links_to FOREIGN KEY(to_item_id) REFERENCES catalog_items(id),
) PRIMARY KEY(id)",
        );
        self.push(
            "CREATE TABLE account_items (
  account_id STRING(36) NOT NULL,
  item_id STRING(64) NOT NULL,
  pinned BOOL NOT NULL DEFAULT (false),
) PRIMARY KEY(account_id, item_id)",
        );
        self.push(
            "CREATE TABLE account_item_status (
  account_id STRING(36) NOT NULL,
  item_id STRING(64) NOT NULL,
  status STRING(32) NOT NULL,
  changed_at TIMESTAMP NOT NULL OPTIONS (allow_commit_timestamp = true),
) PRIMARY KEY(item_id, account_id)",
        );
        self.push("CREATE INDEX idx_account_item_status_account ON account_item_status(account_id, status)");
        self.push(
            "CREATE TABLE audit.deliveries (
  account_id STRING(36) NOT NULL,
  sent_at TIMESTAMP NOT NULL,
  channel STRING(32) NOT NULL,
  payload BYTES(MAX),
) PRIMARY KEY(account_id), ROW DELETION POLICY (OLDER_THAN(sent_at, INTERVAL 1 DAY))",
        );
        self.listings();
    }

    /// Seven region shards with identical columns, each with its own search indexes, plus the
    /// UNION ALL views over them (the production pattern of sharded search tables).
    fn listings(&mut self) {
        let cols = "  id STRING(64) NOT NULL,
  account_id STRING(36),
  name STRING(MAX),
  aliases ARRAY<STRING(MAX)>,
  identifiers ARRAY<STRING(MAX)>,
  primary_country STRING(2),
  countries ARRAY<STRING(2)>,
  listing_kind STRING(16) NOT NULL,
  min_capacity INT64,
  max_capacity INT64,
  location JSON,
  popularity FLOAT64,
  is_stale BOOL NOT NULL DEFAULT (false),
  synced_at TIMESTAMP,";
        let tokens = "
  name_tokens TOKENLIST AS (TOKENIZE_FULLTEXT(name)) HIDDEN,
  aliases_tokens TOKENLIST AS (TOKENIZE_FULLTEXT(aliases)) HIDDEN,
  identifiers_tokens TOKENLIST AS (TOKEN(identifiers)) HIDDEN,
  name_ngram_tokens TOKENLIST AS (TOKENIZE_NGRAMS(name, ngram_size_max => 4, ngram_size_min => 2)) HIDDEN,";
        let storing = "STORING (is_stale, primary_country, listing_kind, countries, popularity, min_capacity, max_capacity)";
        for r in REGIONS {
            self.push(format!(
                "CREATE TABLE listings_{r} (\n{cols}{tokens}\n) PRIMARY KEY(id)"
            ));
            self.push(format!(
                "CREATE SEARCH INDEX idx_listings_{r}_names ON listings_{r}(name_tokens, aliases_tokens, identifiers_tokens) {storing}"
            ));
            if *r != "global" {
                self.push(format!(
                    "CREATE SEARCH INDEX idx_listings_{r}_ngram ON listings_{r}(name_ngram_tokens) {storing}"
                ));
            }
            self.push(format!(
                "CREATE INDEX idx_listings_{r}_account ON listings_{r}(account_id)"
            ));
        }
        let fields = "t.id, t.account_id, t.name, t.aliases, t.identifiers, t.primary_country,\n  t.countries, t.listing_kind, t.min_capacity, t.max_capacity, t.location,\n  t.popularity, t.is_stale, t.synced_at";
        let union = |extra: &str| {
            REGIONS
                .iter()
                .map(|r| format!("SELECT\n  {fields}{extra}\nFROM listings_{r} AS t"))
                .collect::<Vec<_>>()
                .join("\nUNION ALL\n")
        };
        self.push(format!(
            "CREATE VIEW all_listings SQL SECURITY INVOKER AS {}",
            union("")
        ));
        self.push(format!(
            "CREATE VIEW all_listings_search SQL SECURITY INVOKER AS {}",
            union(",\n  t.name_tokens, t.aliases_tokens, t.identifiers_tokens")
        ));
        self.push(
            "CREATE VIEW account_overview SQL SECURITY INVOKER AS SELECT
  a.account_id AS account_id,
  a.name AS name,
  a.plan AS plan,
  ARRAY(
    SELECT s.item_id
    FROM account_item_status AS s
    WHERE s.account_id = a.account_id
  ) AS tracked_items,
  (SELECT COUNT(*) FROM sales.orders AS o WHERE o.account_id = a.account_id) AS order_count
FROM accounts AS a",
        );
        self.push(
            "CREATE VIEW sales.daily_totals SQL SECURITY INVOKER AS SELECT
  o.account_id AS account_id,
  DATE(o.placed_at) AS day,
  SUM(o.total) AS total
FROM sales.orders AS o
GROUP BY o.account_id, DATE(o.placed_at)",
        );
        self.push(
            "CREATE PROPERTY GRAPH item_graph
NODE TABLES (
  accounts,
  catalog_items
)
EDGE TABLES (
  account_items AS TRACKS
    SOURCE KEY (account_id) REFERENCES accounts
    DESTINATION KEY (item_id) REFERENCES catalog_items,

  item_links AS LINKS_TO
    SOURCE KEY (from_item_id) REFERENCES catalog_items
    DESTINATION KEY (to_item_id) REFERENCES catalog_items
)",
        );
        self.push(
            "CREATE CHANGE STREAM order_changes FOR sales.orders, accounts(name, plan) OPTIONS (retention_period = '7d', value_capture_type = 'NEW_ROW')",
        );
        self.push("CREATE CHANGE STREAM everything FOR ALL");
        self.access.extend(
            [
                "CREATE ROLE reader",
                "CREATE ROLE order_clerk",
                "GRANT SELECT ON TABLE accounts, catalog_items TO ROLE reader",
                "GRANT SELECT, INSERT, UPDATE ON TABLE sales.orders TO ROLE order_clerk",
                "GRANT SELECT ON VIEW all_listings TO ROLE reader",
                "GRANT ROLE reader TO ROLE order_clerk",
            ]
            .map(String::from),
        );
    }

    /// One generated table plus its indexes. `parent` is an interleave parent with its key.
    fn table(
        &mut self,
        schema: &str,
        name: &str,
        parent: Option<(&str, &[Col])>,
    ) -> (String, Vec<Col>) {
        let full = qualify(schema, name);
        let fl = flat(&full);
        let rng = &mut self.rng;
        let mut key: Vec<Col> = parent.map(|p| p.1.to_vec()).unwrap_or_default();
        let own_key = Col {
            name: if parent.is_some() {
                format!("{}_id", name.rsplit('_').next().unwrap_or("row"))
            } else {
                "id".into()
            },
            ty: rng.pick(&["STRING(64)", "STRING(36)", "INT64"]),
        };
        // A rare composite key on a root table.
        if parent.is_none() && rng.chance(15) {
            key.push(Col {
                name: "account_id".into(),
                ty: "STRING(36)",
            });
        }
        key.push(own_key);
        let mut lines: Vec<String> = key
            .iter()
            .map(|c| format!("  {} {} NOT NULL,", c.name, c.ty))
            .collect();
        let mut cols: Vec<Col> = Vec::new();
        let n = 5 + rng.below(14);
        let mut used: Vec<String> = key.iter().map(|c| c.name.clone()).collect();
        let has_account = used.iter().any(|c| c == "account_id") || rng.chance(70);
        if has_account && !used.iter().any(|c| c == "account_id") {
            lines.push("  account_id STRING(36) NOT NULL,".into());
            used.push("account_id".into());
            cols.push(Col {
                name: "account_id".into(),
                ty: "STRING(36)",
            });
        }
        for _ in 0..n {
            let w = rng.pick(COLUMN_WORDS);
            if used.iter().any(|u| u == w) {
                continue;
            }
            used.push(w.to_string());
            let ty = if w.starts_with("is_") {
                "BOOL"
            } else if w.ends_with("_at") {
                "TIMESTAMP"
            } else if w.ends_with("_on") {
                "DATE"
            } else if matches!(w, "tags" | "aliases" | "keywords" | "regions") {
                "ARRAY<STRING(MAX)>"
            } else if w == "scores" {
                "ARRAY<FLOAT64>"
            } else if matches!(
                w,
                "payload" | "metadata" | "attributes" | "config" | "options"
            ) {
                "JSON"
            } else if matches!(
                w,
                "attempts" | "retries" | "quantity" | "version_no" | "rank" | "level"
            ) {
                "INT64"
            } else {
                pick_type(rng)
            };
            let mut line = format!("  {w} {ty}");
            if rng.chance(20) {
                line.push_str(" NOT NULL");
            }
            if !ty.starts_with("ARRAY") && rng.chance(12) {
                let d = match ty {
                    "BOOL" => "false",
                    "INT64" => "0",
                    "FLOAT64" => "0.0",
                    "TIMESTAMP" => "CURRENT_TIMESTAMP()",
                    "DATE" => "CURRENT_DATE()",
                    "JSON" => "JSON '{}'",
                    "NUMERIC" => "NUMERIC '0'",
                    t if t.starts_with("STRING") => "''",
                    _ => "NULL",
                };
                line.push_str(&format!(" DEFAULT ({d})"));
            }
            lines.push(format!("{line},"));
            cols.push(Col {
                name: w.to_string(),
                ty,
            });
        }
        if rng.chance(45) {
            lines.push("  created_at TIMESTAMP NOT NULL DEFAULT (CURRENT_TIMESTAMP()),".into());
            cols.push(Col {
                name: "created_at".into(),
                ty: "TIMESTAMP",
            });
        }
        if rng.chance(8) {
            lines.push(
                "  updated_at TIMESTAMP OPTIONS (\n    allow_commit_timestamp = true\n  ),".into(),
            );
            cols.push(Col {
                name: "updated_at".into(),
                ty: "TIMESTAMP",
            });
        }
        // Generated columns: a virtual one now and then, a stored one rarely.
        if let Some(s) = cols.iter().find(|c| c.ty == "STRING(MAX)").cloned()
            && rng.chance(10)
        {
            let stored = if rng.chance(30) { " STORED" } else { "" };
            lines.push(format!(
                "  {}_upper STRING(MAX) AS (UPPER({})){stored},",
                s.name, s.name
            ));
        }
        if has_account && rng.chance(25) {
            let cascade = if rng.chance(50) {
                " ON DELETE CASCADE"
            } else {
                ""
            };
            let suffix = if cascade.is_empty() { "" } else { "_cascade" };
            lines.push(format!(
                "  CONSTRAINT fk_{fl}_account{suffix} FOREIGN KEY(account_id) REFERENCES accounts(account_id){cascade},"
            ));
        }
        if let Some(c) = cols.iter().find(|c| c.ty == "INT64").cloned()
            && rng.chance(5)
        {
            lines.push(format!(
                "  CONSTRAINT ck_{fl}_{}_non_negative CHECK({} >= 0),",
                c.name, c.name
            ));
        }
        let pk = key
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut tail = format!(") PRIMARY KEY({pk})");
        if let Some((p, _)) = parent {
            tail.push_str(&format!(",\n  INTERLEAVE IN PARENT {p} ON DELETE CASCADE"));
        }
        if let Some(ts) = cols.iter().find(|c| c.ty == "TIMESTAMP")
            && rng.chance(2)
        {
            tail.push_str(&format!(
                ", ROW DELETION POLICY (OLDER_THAN({}, INTERVAL 90 DAY))",
                ts.name
            ));
        }
        self.push(format!(
            "CREATE TABLE {full} (\n{}\n{tail}",
            lines.join("\n")
        ));

        // Indexes: on average a little under one per table, with the production mix of
        // plain, UNIQUE, NULL_FILTERED and STORING.
        let mut idx_cols: Vec<&Col> = cols
            .iter()
            .filter(|c| !c.ty.starts_with("ARRAY") && c.ty != "JSON" && c.ty != "BYTES(MAX)")
            .collect();
        let storable: Vec<&Col> = cols.iter().filter(|c| c.ty != "BYTES(MAX)").collect();
        let count = match self.rng.below(100) {
            0..=30 => 0,
            31..=80 => 1,
            81..=95 => 2,
            _ => 3,
        };
        for i in 0..count.min(idx_cols.len()) {
            let lead = idx_cols.remove(self.rng.below(idx_cols.len()));
            let mut on = vec![lead.name.clone()];
            if lead.name != "account_id"
                && cols.iter().any(|c| c.name == "account_id")
                && self.rng.chance(40)
            {
                on.insert(0, "account_id".into());
            }
            if let Some(ts) = cols
                .iter()
                .find(|c| c.ty == "TIMESTAMP" && c.name != lead.name)
                && self.rng.chance(30)
            {
                on.push(format!("{} DESC", ts.name));
            }
            let kind = match self.rng.below(100) {
                0..=6 => "UNIQUE ",
                7..=11 => "NULL_FILTERED ",
                12..=14 => "UNIQUE NULL_FILTERED ",
                _ => "",
            };
            let idx_name = format!("idx_{}_{}", name, lead.name);
            let idx_name = if i > 0 && on.len() > 1 {
                format!("{idx_name}_{}", on.len())
            } else {
                idx_name
            };
            let mut s = format!(
                "CREATE {kind}INDEX {} ON {full}({})",
                qualify(schema, &idx_name),
                on.join(", ")
            );
            if kind.is_empty() && self.rng.chance(15) {
                let st: Vec<&str> = storable
                    .iter()
                    .filter(|c| {
                        !on.iter()
                            .any(|o| o.split(' ').next() == Some(c.name.as_str()))
                    })
                    .take(1 + self.rng.below(4))
                    .map(|c| c.name.as_str())
                    .collect();
                if !st.is_empty() {
                    s.push_str(&format!(" STORING ({})", st.join(", ")));
                }
            }
            if let Some((p, pk)) = parent
                && kind.is_empty()
                && self.rng.chance(50)
            {
                let mut keyed: Vec<String> = pk.iter().map(|c| c.name.clone()).collect();
                keyed.push(lead.name.clone());
                s = format!(
                    "CREATE INDEX {} ON {full}({}), INTERLEAVE IN {p}",
                    qualify(schema, &format!("{idx_name}_by_parent")),
                    keyed.join(", ")
                );
            }
            self.push(s);
        }
        (full, key)
    }

    fn bulk(&mut self, tables: usize) {
        let mut made = 0;
        let mut names: Vec<String> = Vec::new();
        while made < tables {
            let schema = match self.rng.below(10) {
                0..=4 => "",
                n => SCHEMAS[n - 5],
            };
            let noun = self.rng.pick(NOUNS);
            let q = self.rng.pick(QUALIFIERS);
            let name = format!("{noun}_{q}");
            let full = qualify(schema, &name);
            if names.contains(&full) {
                continue;
            }
            names.push(full);
            let (parent, key) = self.table(schema, &name, None);
            made += 1;
            // About one root in ten gets an interleaved child, and some of those a grandchild.
            if made < tables && self.rng.chance(10) {
                let child = format!("{noun}_{}", self.rng.pick(&["entries", "details", "parts"]));
                let full_child = qualify(schema, &child);
                if !names.contains(&full_child) {
                    names.push(full_child);
                    let (c, ck) = self.table(schema, &child, Some((&parent, &key)));
                    made += 1;
                    if made < tables && self.rng.chance(25) {
                        let gc = format!("{noun}_notes");
                        let full_gc = qualify(schema, &gc);
                        if !names.contains(&full_gc) {
                            names.push(full_gc);
                            self.table(schema, &gc, Some((&c, &ck)));
                            made += 1;
                        }
                    }
                }
            }
        }
    }
}

fn main() {
    let mut g = Gen {
        rng: Rng(0x5eed_5ba0_b017_2026),
        ddl: Vec::new(),
        access: Vec::new(),
    };
    g.core();
    g.bulk(240);
    let mut schema = Schema::default();
    for (i, s) in g.ddl.iter().chain(&g.access).enumerate() {
        let stmt = parse_statement(s).unwrap_or_else(|e| panic!("parse #{i}: {e}\n{s}"));
        schema
            .apply(&stmt)
            .unwrap_or_else(|e| panic!("apply #{i}: {e}\n{s}"));
    }
    let out = serde_json::json!({ "ddl": schema.to_ddl(), "access_ddl": g.access });
    println!("{}", serde_json::to_string_pretty(&out).expect("json"));
}
