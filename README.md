# sync-pg2ch

CLI на Rust для проверки совместимости схем PostgreSQL → ClickHouse перед запуском или во время работы CDC. Получает список таблиц из указанной базы ClickHouse, находит их источники в PostgreSQL и выводит diff. Команда `plan` готовит ALTER, `apply` выполняет его после проверки всех таблиц. PostgreSQL остаётся источником метаданных; приложение не управляет CDC.

Утилита рассчитана на append-only передачу с указанием имён колонок. Проверка направленная: например, PostgreSQL `integer` → ClickHouse `Int64` допустима.

## Запуск

Готовый статический бинарник Linux x86_64 доступен в artifact `sync-pg2ch-linux-x86_64-musl` успешного запуска [CI](https://github.com/vadv/sync-pg2ch/actions/workflows/ci.yml). Он не зависит от установленной glibc. После скачивания и распаковки проверьте `sha256sum --check SHA256SUMS`, выполните `chmod +x sync-pg2ch` и запускайте `./sync-pg2ch`.

Сборка из исходников:

```sh
cargo build --release --locked
cp examples/config.toml config.toml

export PG_URL='postgres://reader:password@localhost:5432/app?sslmode=disable'
export CH_URL='http://localhost:8123'
export CH_USER='schema_reader'
export CH_PASSWORD='password'

target/release/sync-pg2ch diff --config config.toml

target/release/sync-pg2ch diff --config config.toml --format json
```

В `config.toml` задайте подключения, `clickhouse.database` и `postgres.schema` (`public` по умолчанию). Перечислять таблицы не нужно: приложение читает их из `system.tables` ClickHouse и ищет источник в выбранной PG-схеме по точному имени и правилу PeerDB `schema_table`. Регистр, точки, кавычки и Unicode в именах сохраняются буквально.

```toml
config_version = 1

[postgres]
url_env = "PG_URL"
schema = "public"

[clickhouse]
url_env = "CH_URL"
database = "analytics"
user_env = "CH_USER"
password_env = "CH_PASSWORD"

[comparison]
ignore_target_columns = ["_peerdb_version", "_peerdb_is_deleted"]
```

Таблица CH без соответствующей таблицы PG даёт `source_table_missing` и exit 1. Таблицы, существующие только в PG, в проверку не входят. Пустая или отсутствующая CH-база даёт exit 2. Системные базы CH выбирать нельзя; временные таблицы не проверяются. Неподдержанные объекты, например View или Distributed, остаются в отчёте со статусом `unknown`.

Для CH-таблицы `public_orders` при `postgres.schema = "public"` приложение проверяет два имени PG: `public.public_orders` и `public.orders`. Если найден один источник, сравнивает его схему; если оба — возвращает `ambiguous_source_table`, `unknown` и exit 2 с перечнем кандидатов. Точное имя не получает приоритета. Префикс снимается целиком: при схеме `sales_eu` имя `sales_eu_order_items` соответствует `sales_eu.order_items`.

Правило `schema.table → schema_table` соответствует [стандартному имени назначения ClickHouse в PeerDB](https://github.com/PeerDB-io/peerdb/blob/8a3660f976c0845bff163ea123581e5cbdd8a5d7/ui/app/mirrors/create/handlers.ts#L383-L420). Произвольные переименования и замена префикса в настройках mirror не поддерживаются. Поиск работает в заданной PG-схеме без обращения к PeerDB.

Служебные и временные копии таблиц можно исключить явно:

```toml
[discovery]
exclude_table_prefixes = ["_peerdb_raw_"]
exclude_tables = ["public_orders_resync"]
```

Имена и префиксы сравниваются буквально с учётом регистра; wildcard-правил нет. По умолчанию исключений нет. Все команды показывают пропущенные таблицы и сработавшее правило в `skipped_tables`; для них PG-источник не ищется и DDL не строится. Если исключены все таблицы, возвращается `no_tables_selected`, exit 2.

Пароли и URL читаются из переменных окружения. Полный пример конфигурации: [examples/config.toml](examples/config.toml). Для PostgreSQL через Unix socket допустим `PG_URL='postgres://reader@/app?host=/var/run/postgresql&sslmode=disable'`.

Для PostgreSQL TLS включён по умолчанию с проверкой сертификата и имени сервера. Для собственного CA используйте URL с `sslmode=verify-full&sslrootcert=/path/ca.pem`. Пример выше явно отключает TLS для локального стенда; режимы с непроверенным сертификатом не поддерживаются.

Для ClickHouse используйте `https://hostname:8443` и при необходимости `ca_cert = "/path/ca.pem"` в секции `[clickhouse]`. Проверка сертификата и имени сервера обязательна для HTTPS. URL CH не должен содержать credentials или query-параметры; пользователь и пароль задаются через `user_env` и `password_env`.

## План и применение ALTER

```sh
target/release/sync-pg2ch plan --config config.toml
target/release/sync-pg2ch plan --config config.toml --format sql
target/release/sync-pg2ch apply --config config.toml --format json
```

`plan` показывает операции и причины блокировки, сохраняя базы без изменений. `apply` строит свежий план, проверяет права и повторно читает метаданные всех таблиц. Если есть хотя бы одна известная проблема, приложение не отправляет ни одного ALTER. При блокировке `plan --format sql` оставляет stdout пустым и пишет причины в stderr.

Поддержаны `ADD COLUMN` и ограниченный набор `MODIFY COLUMN`: расширение signed integer до Int64, Float32 → Float64, Date → Date32, увеличение ёмкости Decimal и добавление Nullable. Более широкий существующий тип сохраняется. Новая колонка получает `Nullable(T)`, даже если PG требует NOT NULL; в старых строках будет NULL. Лишние CH-колонки сохраняются.

DDL выполняется на указанном сервере ClickHouse 25.8 или 26.8 LTS для MergeTree/ReplacingMergeTree в базе Atomic/Ordinary. Изменение ключей и аргументов движка, зависимости, TTL, constraints и неподдержанная топология блокируют план. Независимый `DEFAULT now64()` служебной колонки допускается. Полные [правила DDL](docs/ddl-plan.md) описывают ограничения DEFAULT, codecs, индексов и projections.

На каждую таблицу отправляется один составной ALTER. После него приложение проверяет результат; при ошибке прекращает отправку следующих команд. Отчёт сохраняет `query_id` и состояния `completed`, `failed`, `not_sent`, `indeterminate`. Последнее означает, что исход нужно проверить на сервере перед повторным запуском. Общего rollback для нескольких таблиц нет.

У `plan` и `apply` доступны text/JSON; `report_version = 1`. Exit 0 означает готовый план или подтверждённое выполнение, 1 — известную блокировку до DDL, 2 — ошибку проверки или выполнения. Таймаут одной DDL-команды задаётся отдельно:

```toml
[apply]
ddl_timeout_seconds = 300
```

## Результат diff

Пример при расширении целого типа:

```text
compatible "public"."events" -> "analytics"."events"
  compatible type_widening "id" expected="Int32" actual="Int64"
```

| Статус | Значение | Exit code |
|---|---|---|
| `match` | Совпадение после нормализации | 0 |
| `compatible` | Допустимое отличие: расширение типа, nullable, лишняя CH-колонка | 0 |
| `mismatch` | Известное расхождение схемы | 1 |
| `unknown` | Неподдержанный тип, объект или ошибка проверки | 2 |

При смешанном результате код 2 имеет приоритет над 1. Успешно прочитанные пары сохраняются в отчёте даже при ошибке другой пары. Приложение само не останавливает CDC: вызывающий процесс должен обработать ненулевой exit code.

JSON содержит `report_version = 1`, `policy`, `policy_limitations`, `complete`, `status`, `counts`, `servers`, `tables`, `skipped_tables` и `errors`. Каждый элемент `findings` содержит код результата, имя колонки, исходное определение PG-типа, ожидаемое и фактическое CH-представление. Одна колонка может иметь несколько результатов, например по типу и nullable. JSON включает совпадения и геометаданные; text показывает различия и итог по таблицам. Диагностика идёт в stderr, отчёт — в stdout.

Ошибки подключения указывают этап и причину: неверные credentials, отсутствие прав, отказ соединения или ошибка TLS. Где сервер возвращает код, сохраняется SQLSTATE PostgreSQL или номер ошибки ClickHouse. Полный текст ошибок драйверов и URL с паролями не выводится.

Поле `source` содержит найденный источник или `null`, если источник отсутствует, неоднозначен либо не прочитан. `source_candidates` перечисляет найденных PG-кандидатов. Таблицы в отчёте сортируются по CH-базе и имени, результаты внутри таблицы — по имени колонки и коду.

`complete=false` означает, что полную проверку подтвердить нельзя: есть неподдержанный тип/объект, ошибка чтения или наблюдаемое изменение схемы. Во всех этих случаях возвращаются `status=unknown` и exit 2; успешно найденные отличия сохраняются.

## Правила сравнения

Полная [матрица совместимости](docs/compatibility.md) описывает типы и ограничения. Поддержаны целые, float, Bool, String, UUID, ограниченный numeric/Decimal, date/timestamp, строковые представления JSON/bytea и геометрии.

- Безопасное расширение назначения разрешено; сужение и signed → unsigned дают mismatch.
- Decimal проверяется по исходным precision/scale PG, включая ёмкость целой части.
- Nullable проверяется независимо от базового типа. DEFAULT не заменяет возможность хранить NULL.
- `timestamptz` → `DateTime64(p)` без явной зоны допускается при подходящей точности, если `timezone()` подключения CH возвращает UTC или Etc/UTC. В отчёте — `inherited_timezone` и `default_timezone`. Другая зона даёт mismatch; CDC-подключение должно использовать ту же зону.
- `tsvector` сопоставляется со String или LowCardinality(String); Nullable проверяется отдельно. Содержимое строк не проверяется.
- Все семь встроенных геотипов PG и PostGIS `geometry`/`geography` сопоставляются со String. PostGIS определяется по принадлежности типа расширению. В JSON сохраняются объявленные subtype, SRID и Z/M; CH String эти ограничения не выражает.
- Лишняя CH-колонка даёт `compatible` с кодом `target_only_column`. Исключения в `comparison.ignore_target_columns` скрывают выбранные колонки из diff и применяются ко всем найденным таблицам. Исключение не может скрыть колонку PG.
- Порядок колонок игнорируется. Переименование отображается как отсутствующая и лишняя колонки. ALIAS/MATERIALIZED вместо исходной колонки дают mismatch.
- Domains, enum, массивы, unconstrained numeric, generated columns и прочие неподдержанные случаи дают unknown.

Проверяются постоянные обычные PG-таблицы и родительские partitioned tables, в CH — распознанные движки семейства MergeTree. Разделы PG не обходятся. Команда `diff` не сравнивает ограничения, индексы, выражения DEFAULT, ключи сортировки, TTL и codecs; `plan/apply` читают эти определения для проверки допустимости ALTER.

Приложение читает каталоги дважды и сообщает `schema_changed_during_check`, если прочитанные метаданные изменились. Изменение набора PG-кандидатов даёт `source_mapping_changed_during_check`, а добавление или удаление CH-таблицы — `table_set_changed_during_check`. Все три случая дают exit 2 с сохранением уже найденных различий. Это не атомарный снимок двух серверов и не защита от DDL после завершения проверки.

Диапазоны дат, специальные числовые значения и формат String зависят от сериализации CDC и не проверяются по метаданным. Произвольные CHECK-ограничения PostGIS и история изменений SRID не восстанавливаются.

## Права

PostgreSQL: подключение к базе, чтение `pg_catalog` и выполнение стандартных функций метаданных; для PostGIS — доступ к namespace расширения и его typmod-функциям. Стандартный LOGIN-пользователь с обычными PUBLIC-привилегиями на каталоги подходит. Клиент дополнительно включает `default_transaction_read_only=on`.

ClickHouse: все команды требуют `SHOW TABLES, SHOW COLUMNS ON *.*`. Глобальная видимость метаданных нужна, чтобы отличать отсутствующую таблицу от скрытой из-за прав. При её отсутствии приложение возвращает exit 2. Чтение каталогов использует `readonly=2`.

Для `plan/apply` дополнительно нужны `SELECT ON system.data_skipping_indices`, `system.projections`, `system.mutations` и `system.processes`. Для `apply` выдайте `ALTER ADD COLUMN` и/или `ALTER MODIFY COLUMN` на изменяемые таблицы. Проверка ALTER-прав и выполнение используют `readonly=0`; профиль пользователя должен разрешать это значение. `plan` не проверяет ALTER-права, поэтому его можно запускать с пользователем для чтения метаданных.

Приложению не нужны SELECT пользовательских строк и права на изменение PG. Интеграционные тесты проверяют ограниченные права и блокировку всего apply при нехватке прав на последнюю таблицу.

## Разработка и CI

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --lib --bins
```

[GitHub Actions](.github/workflows/ci.yml) запускает Docker service containers: PostgreSQL 14.24 и PostgreSQL 14.24 + PostGIS 3.6.4, каждый с ClickHouse 25.8.23.13 и 26.8.36.3 LTS. Образы закреплены digest в workflow; Rust для CI — 1.99.1. Во всех четырёх сочетаниях проверяется один статический бинарник из artifact, включая TLS и подключение PG через Unix socket.

Локальная статическая сборка требует `musl-gcc` (пакет `musl-tools` в Debian/Ubuntu):

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
```

Для проверки готового бинарника задайте `SYNC_PG2CH_TEST_BINARY=/absolute/path/sync-pg2ch`; без этой переменной тесты используют бинарник, собранный Cargo.

Локальный запуск той же матрицы требует самостоятельно поднятых тестовых серверов и административных подключений:

```sh
PG_URL='postgres://postgres:test-pass@localhost:5432/postgres?sslmode=disable' \
CH_URL='http://localhost:8123' CH_USER=default CH_PASSWORD=test-pass \
TEST_POSTGIS=false TEST_ARTIFACTS=test-results \
cargo test --locked --features integration --test integration -- --nocapture
```

Для сервера с PostGIS задайте `TEST_POSTGIS=true`. Без адресов серверов тест завершается с ошибкой. Тестовый код создаёт отдельные базы и пользователей, применяет SQL-fixtures и удаляет их после успешного прогона. Используйте отдельные тестовые серверы.

TLS-тест запускается отдельно после настройки сервисов скриптом `tests/fixtures/configure-tls.sh`, как в workflow. Он проверяет доверенный CA, недоверенный сертификат и несовпадение hostname у обеих СУБД, а также plan/apply через TLS. В обычном локальном прогоне этот тест помечен ignored; в CI его явный запуск обязателен.

DDL-тесты запускают бинарник через HTTP-прокси с записью запросов. Они проверяют отсутствие ALTER при блокировках, сохранение старых значений и атрибутов, повторный запуск, гонку метаданных, отказ второй команды и потерю ответа после её выполнения.
