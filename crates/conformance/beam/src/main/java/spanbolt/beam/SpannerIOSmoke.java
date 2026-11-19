package spanbolt.beam;

import com.google.cloud.ByteArray;
import com.google.cloud.NoCredentials;
import com.google.cloud.Timestamp;
import com.google.cloud.spanner.DatabaseAdminClient;
import com.google.cloud.spanner.DatabaseClient;
import com.google.cloud.spanner.DatabaseId;
import com.google.cloud.spanner.InstanceAdminClient;
import com.google.cloud.spanner.InstanceConfigId;
import com.google.cloud.spanner.InstanceId;
import com.google.cloud.spanner.InstanceInfo;
import com.google.cloud.spanner.Key;
import com.google.cloud.spanner.KeyRange;
import com.google.cloud.spanner.KeySet;
import com.google.cloud.spanner.Mutation;
import com.google.cloud.spanner.ResultSet;
import com.google.cloud.spanner.Spanner;
import com.google.cloud.spanner.SpannerException;
import com.google.cloud.spanner.SpannerOptions;
import com.google.cloud.spanner.Statement;
import com.google.cloud.spanner.Struct;
import com.google.cloud.spanner.Value;
import java.math.BigDecimal;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HexFormat;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.function.Consumer;
import org.apache.beam.runners.direct.DirectRunner;
import org.apache.beam.sdk.Pipeline;
import org.apache.beam.sdk.coders.SerializableCoder;
import org.apache.beam.sdk.io.gcp.spanner.MutationGroup;
import org.apache.beam.sdk.io.gcp.spanner.SpannerConfig;
import org.apache.beam.sdk.io.gcp.spanner.SpannerIO;
import org.apache.beam.sdk.io.gcp.spanner.SpannerWriteResult;
import org.apache.beam.sdk.options.PipelineOptions;
import org.apache.beam.sdk.options.PipelineOptionsFactory;
import org.apache.beam.sdk.options.ValueProvider.StaticValueProvider;
import org.apache.beam.sdk.transforms.Create;
import org.apache.beam.sdk.transforms.DoFn;
import org.apache.beam.sdk.transforms.ParDo;
import org.apache.beam.sdk.values.PCollection;

/**
 * Beam SpannerIO smoke test. Runs DirectRunner pipelines that read and write through
 * SpannerIO the way typical Beam jobs do, against one endpoint, and writes one JSON line per step:
 * the step name, a row count, a digest of the sorted rows (or the failures it saw) and the time it
 * took. Run it once against spanbolt and once against the official emulator, then compare the two
 * files with scripts/beam_smoke_compare.py. Usage: SpannerIOSmoke HOST:PORT OUT.jsonl
 */
public final class SpannerIOSmoke {
  static final String PROJECT = "test-project";
  static final String INSTANCE = "beam-smoke";
  static final int CUSTOMERS = 5000;
  static final int CHANGES_PER_CUSTOMER = 3;
  static final String[] REGIONS = {"eu", "us", "apac", "latam", "mea", "nordics", "uk"};
  static final String[] KINDS = {"k0", "k1", "k2", "k3", "k4"};

  /** Rows collected by the pipelines' sink DoFns; the DirectRunner runs in this JVM. */
  static final Map<String, ConcurrentLinkedQueue<String>> SINK = new ConcurrentHashMap<>();

  static final List<String> DDL =
      List.of(
          "CREATE TABLE Customers ("
              + " CustomerId INT64 NOT NULL,"
              + " Name STRING(MAX),"
              + " Region STRING(16),"
              + " Score FLOAT64,"
              + " Created TIMESTAMP,"
              + " Tags ARRAY<STRING(MAX)>,"
              + " Payload JSON,"
              + " Data BYTES(MAX),"
              + ") PRIMARY KEY (CustomerId)",
          "CREATE INDEX CustomersByRegion ON Customers(Region) STORING (Name)",
          "CREATE TABLE Changes ("
              + " CustomerId INT64 NOT NULL,"
              + " ChangeId STRING(64) NOT NULL,"
              + " Kind STRING(16) NOT NULL,"
              + " ComputedAt TIMESTAMP NOT NULL,"
              + " Value NUMERIC,"
              + " Note STRING(MAX),"
              + " Active BOOL,"
              + ") PRIMARY KEY (CustomerId, ChangeId),"
              + " INTERLEAVE IN PARENT Customers ON DELETE CASCADE",
          "CREATE INDEX ChangesByKind ON Changes(Kind, ComputedAt DESC) STORING (Value)",
          "CREATE INDEX ChangesByComputedAt ON Changes(ComputedAt)",
          "CREATE UNIQUE NULL_FILTERED INDEX ChangesByNote ON Changes(CustomerId, Note),"
              + " INTERLEAVE IN Customers");

  final String host;
  final SpannerConfig config;
  final DatabaseClient client;
  final List<String> out = new ArrayList<>();

  SpannerIOSmoke(String host, String database, DatabaseClient client) {
    this.host = host;
    this.client = client;
    this.config =
        SpannerConfig.create()
            .withProjectId(PROJECT)
            .withInstanceId(INSTANCE)
            .withDatabaseId(database)
            .withEmulatorHost(StaticValueProvider.of(host));
  }

  public static void main(String[] args) throws Exception {
    String host = args[0];
    Path outFile = Path.of(args[1]);
    String database = "smoke" + Long.toString(System.currentTimeMillis() % 1_000_000_000L, 36);
    SpannerOptions options =
        SpannerOptions.newBuilder()
            .setProjectId(PROJECT)
            .setEmulatorHost(host)
            .setCredentials(NoCredentials.getInstance())
            .build();
    try (Spanner spanner = options.getService()) {
      InstanceAdminClient instances = spanner.getInstanceAdminClient();
      try {
        instances
            .createInstance(
                InstanceInfo.newBuilder(InstanceId.of(PROJECT, INSTANCE))
                    .setInstanceConfigId(InstanceConfigId.of(PROJECT, "emulator-config"))
                    .setDisplayName(INSTANCE)
                    .setNodeCount(1)
                    .build())
            .get();
      } catch (Exception e) {
        if (!String.valueOf(e.getMessage()).contains("ALREADY_EXISTS")) {
          throw e;
        }
      }
      DatabaseAdminClient admin = spanner.getDatabaseAdminClient();
      long t0 = System.nanoTime();
      admin.createDatabase(INSTANCE, database, DDL).get();
      long ddlMs = (System.nanoTime() - t0) / 1_000_000;
      DatabaseClient client = spanner.getDatabaseClient(DatabaseId.of(PROJECT, INSTANCE, database));
      SpannerIOSmoke smoke = new SpannerIOSmoke(host, database, client);
      smoke.record("setup.create_database", Map.of("statements", DDL.size()), ddlMs);
      try {
        smoke.run();
      } finally {
        Files.write(outFile, smoke.out, StandardCharsets.UTF_8);
        try {
          admin.dropDatabase(INSTANCE, database);
        } catch (SpannerException ignored) {
          // Best effort; the endpoint is thrown away after the run anyway.
        }
      }
    }
  }

  void run() throws Exception {
    // 1. Plain SpannerIO.write with the defaults: the parent rows.
    List<Mutation> parents = new ArrayList<>();
    for (int i = 0; i < CUSTOMERS; i++) {
      parents.add(customer(Mutation.newInsertBuilder("Customers"), i).build());
    }
    write("write.default.customers", parents, SpannerIO.write().withSpannerConfig(config));

    // 2. ComputeChangeFacts.writeChangeLog's settings, into the interleaved table with indexes.
    List<Mutation> children = new ArrayList<>();
    for (int i = 0; i < CUSTOMERS; i++) {
      for (int j = 0; j < CHANGES_PER_CUSTOMER; j++) {
        children.add(change(Mutation.newInsertOrUpdateBuilder("Changes"), i, j).build());
      }
    }
    write("write.changefacts.changes", children, changeFactsWriter());

    // 3. The same writer over a mix: updates on both tables and deletes of children and parents
    //    (deleting a parent cascades to its children and their index entries).
    List<Mutation> mixed = new ArrayList<>();
    // No key is touched twice: the order of mutations on one key inside a batch is not defined.
    for (int i = 0; i < CUSTOMERS; i += 5) {
      if (i % 97 == 3) {
        continue;
      }
      mixed.add(
          Mutation.newUpdateBuilder("Customers")
              .set("CustomerId").to(i)
              .set("Score").to(-i * 1.5)
              .set("Tags").toStringArray(Arrays.asList("updated", null))
              .build());
    }
    for (int i = 1; i < CUSTOMERS; i += 7) {
      if (i % 97 == 3) {
        continue;
      }
      mixed.add(
          Mutation.newReplaceBuilder("Changes")
              .set("CustomerId").to(i)
              .set("ChangeId").to(changeId(i, 0))
              .set("Kind").to("k9")
              .set("ComputedAt").to(Timestamp.ofTimeSecondsAndNanos(1_800_000_000L + i, 123_456_000))
              .build());
    }
    for (int i = 2; i < CUSTOMERS; i += 11) {
      mixed.add(Mutation.delete("Changes", Key.of(i, changeId(i, 1))));
    }
    for (int i = 3; i < CUSTOMERS; i += 97) {
      mixed.add(Mutation.delete("Customers", Key.of(i)));
    }
    mixed.add(Mutation.delete("Changes", KeySet.range(KeyRange.closedOpen(Key.of(4000), Key.of(4010)))));
    write("write.changefacts.mixed", mixed, changeFactsWriter());

    state("state.after_writes");

    // 4. withQuery, unbatched (MonitoredEntitiesIO, CustomRelationResolver) and batched
    //    (GraniteChangeTracking: BatchReadOnlyTransaction + PartitionQuery + partition tokens).
    String customers = "SELECT * FROM Customers";
    String changes =
        "SELECT CustomerId, ChangeId, Kind, ComputedAt, Value, Note, Active FROM Changes"
            + " WHERE Kind != 'k0'";
    String toJson = "SELECT TO_JSON(data) AS json FROM Customers data WHERE Region != 'eu'";
    for (boolean batching : new boolean[] {false, true}) {
      String mode = batching ? "batched" : "unbatched";
      read("read.query.customers." + mode, SpannerIO.read().withQuery(customers).withBatching(batching));
      read("read.query.changes." + mode, SpannerIO.read().withQuery(changes).withBatching(batching));
      read("read.query.to_json." + mode, SpannerIO.read().withQuery(toJson).withBatching(batching));
      read(
          "read.query.params." + mode,
          SpannerIO.read()
              .withQuery(
                  Statement.newBuilder(
                          "SELECT CustomerId, Name FROM Customers WHERE Region = @region AND"
                              + " CustomerId >= @min")
                      .bind("region").to("apac")
                      .bind("min").to(1000L)
                      .build())
              .withBatching(batching));
    }

    // 5. withTable/withColumns, through the primary key and through secondary indexes.
    for (boolean batching : new boolean[] {false, true}) {
      String mode = batching ? "batched" : "unbatched";
      read(
          "read.table.customers_by_region." + mode,
          SpannerIO.read()
              .withTable("Customers")
              .withColumns("CustomerId", "Region", "Name")
              .withIndex("CustomersByRegion")
              .withBatching(batching));
      read(
          "read.table.customers_by_region.prefix." + mode,
          SpannerIO.read()
              .withTable("Customers")
              .withColumns("CustomerId", "Region", "Name")
              .withIndex("CustomersByRegion")
              .withKeySet(KeySet.newBuilder().addKey(Key.of("nordics")).addKey(Key.of("uk")).build())
              .withBatching(batching));
      read(
          "read.table.changes_by_kind.range." + mode,
          SpannerIO.read()
              .withTable("Changes")
              .withColumns("CustomerId", "ChangeId", "Kind", "ComputedAt", "Value")
              .withIndex("ChangesByKind")
              .withKeySet(KeySet.range(KeyRange.closedClosed(Key.of("k1"), Key.of("k3"))))
              .withBatching(batching));
      read(
          "read.table.changes.pk_range." + mode,
          SpannerIO.read()
              .withTable("Changes")
              .withColumns("CustomerId", "ChangeId", "Kind", "Note", "Active")
              .withKeySet(KeySet.range(KeyRange.closedOpen(Key.of(100), Key.of(600))))
              .withBatching(batching));
    }

    // 6. Grouped writes (BqToSpannerSync) with REPORT_FAILURES and a few failing groups. A batch
    //    that fails is retried group by group; only the failing groups come back.
    List<MutationGroup> groups = new ArrayList<>();
    for (int i = 0; i < 300; i++) {
      int id = 10_000 + i;
      groups.add(
          MutationGroup.create(
              customer(Mutation.newInsertOrUpdateBuilder("Customers"), id).build(),
              change(Mutation.newInsertOrUpdateBuilder("Changes"), id, 0).build(),
              change(Mutation.newInsertOrUpdateBuilder("Changes"), id, 1).build()));
    }
    Map<String, MutationGroup> failing = new LinkedHashMap<>();
    // A child whose parent does not exist (NOT_FOUND); the parent insert in the group rolls back.
    failing.put(
        "missing_parent",
        MutationGroup.create(
            customer(Mutation.newInsertBuilder("Customers"), 20_000).build(),
            change(Mutation.newInsertBuilder("Changes"), 999_999, 0).build()));
    // Insert of an existing row (ALREADY_EXISTS).
    failing.put(
        "duplicate_insert",
        MutationGroup.create(customer(Mutation.newInsertBuilder("Customers"), 1).build()));
    // The failing groups touch only rows written before this step, so the outcome does not
    // depend on the order SpannerIO retries the groups of a failed batch in.
    // A NOT NULL column left out.
    failing.put(
        "null_not_null",
        MutationGroup.create(
            Mutation.newInsertOrUpdateBuilder("Changes")
                .set("CustomerId").to(11)
                .set("ChangeId").to("missing-kind")
                .set("ComputedAt").to(Timestamp.ofTimeSecondsAndNanos(1_700_000_000L, 0))
                .build()));
    // A unique index violation: the same (CustomerId, Note) as an existing row.
    failing.put(
        "unique_index",
        MutationGroup.create(
            Mutation.newInsertOrUpdateBuilder("Changes")
                .set("CustomerId").to(10)
                .set("ChangeId").to("dup-note")
                .set("Kind").to("k1")
                .set("ComputedAt").to(Timestamp.ofTimeSecondsAndNanos(1_700_000_000L, 0))
                .set("Note").to(note(10, 1))
                .build()));
    groups.addAll(failing.values());
    writeGrouped("write.grouped.report_failures", groups);

    // The same failing groups through the client directly, for the error code and message.
    for (Map.Entry<String, MutationGroup> e : failing.entrySet()) {
      Map<String, Object> f = new LinkedHashMap<>();
      long t = System.nanoTime();
      try {
        List<Mutation> ms = new ArrayList<>();
        e.getValue().forEach(ms::add);
        client.writeAtLeastOnce(ms);
        f.put("error", null);
      } catch (SpannerException ex) {
        f.putAll(error(ex));
      }
      record("error." + e.getKey(), f, (System.nanoTime() - t) / 1_000_000);
    }

    state("state.final");
  }

  SpannerIO.Write changeFactsWriter() {
    return SpannerIO.write()
        .withSpannerConfig(config)
        .withLowPriority()
        .withMaxCommitDelay(100)
        .withBatchSizeBytes(1024 * 1024 * 5)
        .withMaxNumRows(2000)
        .withMaxNumMutations(80000)
        .withGroupingFactor(250)
        .withFailureMode(SpannerIO.FailureMode.REPORT_FAILURES);
  }

  static String changeId(int customer, int j) {
    return String.format("c-%05d-%d", customer, j);
  }

  static String note(int customer, int j) {
    return (customer + j) % 6 == 0 ? null : "note " + customer + "/" + j;
  }

  static Mutation.WriteBuilder customer(Mutation.WriteBuilder b, int i) {
    b.set("CustomerId").to(i)
        .set("Name").to(i % 41 == 0 ? null : String.format("customer-%05d æøå", i))
        .set("Region").to(i % 50 == 0 ? null : REGIONS[i % REGIONS.length])
        .set("Score").to(i % 13 == 0 ? Double.NaN : i * 0.25)
        .set("Created").to(Timestamp.ofTimeSecondsAndNanos(1_700_000_000L + i * 37L, (i % 1000) * 1000))
        .set("Payload").to(Value.json("{\"i\":" + i + ",\"even\":" + (i % 2 == 0) + ",\"s\":\"x" + i + "\"}"))
        .set("Data").to(ByteArray.copyFrom(new byte[] {(byte) i, (byte) (i >> 8), 0, -1}));
    List<String> tags = new ArrayList<>();
    for (int k = 0; k < i % 4; k++) {
      tags.add(k == 2 ? null : "t" + (i + k) % 9);
    }
    b.set("Tags").toStringArray(i % 17 == 0 ? null : tags);
    return b;
  }

  static Mutation.WriteBuilder change(Mutation.WriteBuilder b, int i, int j) {
    return b.set("CustomerId").to(i)
        .set("ChangeId").to(changeId(i, j))
        .set("Kind").to(KINDS[(i + j) % KINDS.length])
        .set("ComputedAt").to(Timestamp.ofTimeSecondsAndNanos(1_750_000_000L + i * 11L + j, j * 1000))
        .set("Value").to(new BigDecimal(i + "." + String.format("%03d", j * 125)))
        .set("Note").to(note(i, j))
        .set("Active").to((i + j) % 3 == 0 ? null : (i + j) % 3 == 1);
  }

  // --- pipelines -------------------------------------------------------------------------------

  Pipeline pipeline() {
    PipelineOptions options = PipelineOptionsFactory.create();
    options.setRunner(DirectRunner.class);
    return Pipeline.create(options);
  }

  void write(String step, List<Mutation> mutations, SpannerIO.Write writer) {
    Pipeline p = pipeline();
    SpannerWriteResult result =
        p.apply(Create.of(mutations).withCoder(SerializableCoder.of(Mutation.class))).apply(writer);
    collectFailures(step, result);
    long t = System.nanoTime();
    if (!finish(step, p, t)) {
      return;
    }
    Map<String, Object> f = new LinkedHashMap<>();
    f.put("mutations", mutations.size());
    f.put("failed_groups", sorted(step));
    record(step, f, (System.nanoTime() - t) / 1_000_000);
  }

  void writeGrouped(String step, List<MutationGroup> groups) {
    Pipeline p = pipeline();
    SpannerIO.Write writer =
        SpannerIO.write()
            .withSpannerConfig(config)
            .withLowPriority()
            .withMaxCommitDelay(100)
            .withBatchSizeBytes(1024 * 1024 * 5)
            .withMaxNumRows(50000)
            .withMaxNumMutations(80000)
            .withGroupingFactor(2000)
            .withFailureMode(SpannerIO.FailureMode.REPORT_FAILURES);
    SpannerWriteResult result =
        p.apply(Create.of(groups).withCoder(SerializableCoder.of(MutationGroup.class)))
            .apply(new SpannerIO.WriteGrouped(writer));
    collectFailures(step, result);
    long t = System.nanoTime();
    if (!finish(step, p, t)) {
      return;
    }
    Map<String, Object> f = new LinkedHashMap<>();
    f.put("groups", groups.size());
    f.put("failed_groups", sorted(step));
    record(step, f, (System.nanoTime() - t) / 1_000_000);
  }

  void collectFailures(String step, SpannerWriteResult result) {
    SINK.put(step, new ConcurrentLinkedQueue<>());
    result
        .getFailedMutations()
        .apply(
            "Failures",
            ParDo.of(new CollectFailures(step)));
  }

  /** Static DoFns: an anonymous one would capture (and try to serialise) the smoke object. */
  static final class CollectFailures extends DoFn<MutationGroup, Void> {
    final String step;

    CollectFailures(String step) {
      this.step = step;
    }

    @ProcessElement
    public void process(@Element MutationGroup g) {
      List<String> ms = new ArrayList<>();
      for (Mutation m : g) {
        ms.add(m.getOperation() + " " + m.getTable() + " " + keyOf(m));
      }
      SINK.get(step).add(String.join(" + ", ms));
    }
  }

  static final class CollectRows extends DoFn<Struct, Void> {
    final String step;

    CollectRows(String step) {
      this.step = step;
    }

    @ProcessElement
    public void process(@Element Struct row) {
      SINK.get(step).add(render(row));
    }
  }

  static String keyOf(Mutation m) {
    if (m.getOperation() == Mutation.Op.DELETE) {
      return m.getKeySet().toString();
    }
    Map<String, Value> values = m.asMap();
    Value id = values.get("CustomerId");
    Value change = values.get("ChangeId");
    return change == null ? "(" + id + ")" : "(" + id + "," + change + ")";
  }

  void read(String step, SpannerIO.Read reader) {
    Pipeline p = pipeline();
    SINK.put(step, new ConcurrentLinkedQueue<>());
    PCollection<Struct> rows = p.apply(reader.withSpannerConfig(config));
    rows.apply(
        "Collect",
        ParDo.of(new CollectRows(step)));
    long t = System.nanoTime();
    if (!finish(step, p, t)) {
      return;
    }
    record(step, digest(sorted(step)), (System.nanoTime() - t) / 1_000_000);
  }

  /** Reads the tables directly with the client, so writes are checked independently of SpannerIO. */
  void state(String step) {
    String[][] queries = {
      {"customers", "SELECT * FROM Customers"},
      {"changes", "SELECT * FROM Changes"},
      {"customers_by_region", "SELECT CustomerId, Region, Name FROM Customers@{FORCE_INDEX=CustomersByRegion}"},
      {"changes_by_kind", "SELECT CustomerId, ChangeId, Kind, ComputedAt, Value FROM Changes@{FORCE_INDEX=ChangesByKind}"},
      {"changes_by_computed_at", "SELECT CustomerId, ChangeId, ComputedAt FROM Changes@{FORCE_INDEX=ChangesByComputedAt}"},
      {"changes_by_note", "@{spanner_emulator.disable_query_null_filtered_index_check=true}"
            + " SELECT CustomerId, Note, ChangeId FROM Changes@{FORCE_INDEX=ChangesByNote}"
            + " WHERE Note IS NOT NULL"},
    };
    for (String[] q : queries) {
      List<String> rows = new ArrayList<>();
      long t = System.nanoTime();
      try (ResultSet rs = client.singleUse().executeQuery(Statement.of(q[1]))) {
        while (rs.next()) {
          rows.add(render(rs.getCurrentRowAsStruct()));
        }
      } catch (SpannerException e) {
        record(step + "." + q[0], error(e), (System.nanoTime() - t) / 1_000_000);
        continue;
      }
      Collections.sort(rows);
      record(step + "." + q[0], digest(rows), (System.nanoTime() - t) / 1_000_000);
    }
    // The Read RPC the table reads use, without Beam: separates server time from DirectRunner's.
    List<String> rows = new ArrayList<>();
    long t = System.nanoTime();
    try (ResultSet rs =
        client
            .singleUse()
            .readUsingIndex(
                "Changes",
                "ChangesByKind",
                KeySet.range(KeyRange.closedClosed(Key.of("k1"), Key.of("k3"))),
                List.of("CustomerId", "ChangeId", "Kind", "ComputedAt", "Value"))) {
      while (rs.next()) {
        rows.add(render(rs.getCurrentRowAsStruct()));
      }
    }
    Collections.sort(rows);
    record(step + ".read_changes_by_kind_range", digest(rows), (System.nanoTime() - t) / 1_000_000);
  }

  /** Runs the pipeline; on failure records the error as the step's result and returns false. */
  boolean finish(String step, Pipeline p, long t) {
    try {
      p.run().waitUntilFinish();
      return true;
    } catch (RuntimeException e) {
      Throwable c = e;
      while (c.getCause() != null && !(c instanceof SpannerException)) {
        c = c.getCause();
      }
      Map<String, Object> f =
          c instanceof SpannerException se
              ? error(se)
              : Map.of("error", c.getClass().getSimpleName() + ": " + c.getMessage());
      record(step, f, (System.nanoTime() - t) / 1_000_000);
      return false;
    }
  }

  static Map<String, Object> error(SpannerException ex) {
    Map<String, Object> f = new LinkedHashMap<>();
    f.put("code", ex.getErrorCode().name());
    f.put(
        "message",
        ex.getMessage()
            .replaceFirst("^([A-Z_]+: )?(io.grpc.StatusRuntimeException: )?([A-Z_]+: )?", "")
            .replaceFirst(" - RequestId: .*$", ""));
    return f;
  }

  // --- output ----------------------------------------------------------------------------------

  static String render(Struct row) {
    StringBuilder sb = new StringBuilder();
    for (int i = 0; i < row.getColumnCount(); i++) {
      if (i > 0) {
        sb.append('|');
      }
      sb.append(row.getType().getStructFields().get(i).getName()).append('=');
      if (row.isNull(i)) {
        sb.append("NULL");
        continue;
      }
      // Value.toString() shortens long strings, so render those types in full.
      switch (row.getColumnType(i).getCode()) {
        case STRING -> sb.append(row.getString(i));
        case JSON -> sb.append(row.getJson(i));
        case BYTES -> sb.append(row.getBytes(i).toBase64());
        default -> sb.append(row.getValue(i).toString());
      }
    }
    return sb.toString();
  }

  static List<String> sorted(String step) {
    List<String> rows = new ArrayList<>(SINK.get(step));
    Collections.sort(rows);
    return rows;
  }

  static Map<String, Object> digest(List<String> rows) {
    Map<String, Object> f = new LinkedHashMap<>();
    f.put("rows", rows.size());
    try {
      MessageDigest md = MessageDigest.getInstance("SHA-256");
      rows.forEach(r -> md.update((r + "\n").getBytes(StandardCharsets.UTF_8)));
      f.put("sha256", HexFormat.of().formatHex(md.digest()).substring(0, 16));
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
    f.put("first", rows.isEmpty() ? null : rows.get(0));
    f.put("last", rows.isEmpty() ? null : rows.get(rows.size() - 1));
    return f;
  }

  void record(String step, Map<String, Object> fields, long ms) {
    StringBuilder sb = new StringBuilder("{\"step\":").append(json(step));
    fields.forEach((k, v) -> sb.append(',').append(json(k)).append(':').append(json(v)));
    sb.append(",\"ms\":").append(ms).append('}');
    out.add(sb.toString());
    System.out.println(sb);
  }

  static String json(Object v) {
    if (v == null) {
      return "null";
    }
    if (v instanceof Number || v instanceof Boolean) {
      return v.toString();
    }
    if (v instanceof List<?> list) {
      StringBuilder sb = new StringBuilder("[");
      Consumer<Object> add = x -> sb.append(sb.length() > 1 ? "," : "").append(json(x));
      list.forEach(add);
      return sb.append(']').toString();
    }
    String s = v.toString();
    StringBuilder sb = new StringBuilder("\"");
    for (char c : s.toCharArray()) {
      switch (c) {
        case '"' -> sb.append("\\\"");
        case '\\' -> sb.append("\\\\");
        case '\n' -> sb.append("\\n");
        default -> {
          if (c < 0x20) {
            sb.append(String.format("\\u%04x", (int) c));
          } else {
            sb.append(c);
          }
        }
      }
    }
    return sb.append('"').toString();
  }
}
