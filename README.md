# sync-pg2ch

CLI на Rust для проверки совместимости схем PostgreSQL → ClickHouse перед запуском или во время работы CDC. Принимает подключения, автоматически получает таблицы указанной базы ClickHouse и выводит diff с PostgreSQL в text или JSON. Не генерирует миграции, не меняет схему и не управляет CDC.

Первая версия рассчитана на append-only передачу с указанием имён колонок. Проверка направленная: например, PostgreSQL `integer` → ClickHouse `Int64` допустима. Совпадение названий SQL-типов между СУБД не требуется.

## Запуск

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

Секция `[[tables]]` больше не используется. Таблица CH без соответствующей таблицы PG даёт `source_table_missing` и exit 1. Таблицы, существующие только в PG, в проверку не входят. Пустая или отсутствующая CH-база даёт exit 2, чтобы пустой результат не считался успешной проверкой. Системные базы CH выбирать нельзя; временные таблицы не проверяются. Неподдержанные объекты, например View или Distributed, остаются в отчёте со статусом unknown.

Для CH-таблицы `public_orders` при `postgres.schema = "public"` приложение проверяет два имени PG: `public.public_orders` и `public.orders`. Если найден один источник, сравнивает его схему; если оба — возвращает `ambiguous_source_table`, `unknown` и exit 2 с перечнем кандидатов. Точное имя не получает приоритета. Префикс снимается целиком: при схеме `sales_eu` имя `sales_eu_order_items` соответствует `sales_eu.order_items`.

Это стандартное имя назначения ClickHouse в [PeerDB](https://github.com/PeerDB-io/peerdb/blob/8a3660f976c0845bff163ea123581e5cbdd8a5d7/ui/app/mirrors/create/handlers.ts#L383-L420); такое же правило есть в [версии 2024 года](https://github.com/PeerDB-io/peerdb/blob/e92669d751259750c0e3476b053b9cb4a8aebefc/ui/app/mirrors/create/handlers.ts#L378-L414). Произвольное переименование назначения или изменение префикса в настройках mirror восстановить по одному имени нельзя: такие mappings требуют отдельной поддержки. Приложение не обращается к PeerDB и не ищет таблицы в других PG-схемах.

Пароли и URL читаются из переменных окружения. Полный пример конфигурации: [examples/config.toml](examples/config.toml).

Для PostgreSQL TLS включён по умолчанию с проверкой сертификата и имени сервера. Для собственного CA используйте URL с `sslmode=verify-full&sslrootcert=/path/ca.pem`. Пример выше явно отключает TLS для локального стенда; режимы с непроверенным сертификатом не поддерживаются.

Для ClickHouse используйте `https://hostname:8443` и при необходимости `ca_cert = "/path/ca.pem"` в секции `[clickhouse]`. Проверка сертификата и имени сервера обязательна для HTTPS. URL CH не должен содержать credentials или query-параметры; пользователь и пароль задаются через `user_env` и `password_env`.

## Результат

Пример при расширении целого типа:

```text
compatible "public"."events" -> "analytics"."events"
  compatible type_widening "id" expected="Int32" actual="Int64"
```

| Статус | Значение | Exit code |
|---|---|---|
| `match` | Совпадение после нормализации | 0 |
| `compatible` | Разрешённое расширение типа или nullable | 0 |
| `mismatch` | Известное расхождение схемы | 1 |
| `unknown` | Неподдержанный тип, объект или ошибка проверки | 2 |

При смешанном результате код 2 имеет приоритет над 1. Успешно прочитанные пары сохраняются в отчёте даже при ошибке другой пары. Приложение само не останавливает CDC: вызывающий процесс должен обработать ненулевой exit code.

JSON содержит `report_version`, `policy`, `policy_limitations`, `complete`, `status`, `counts`, `servers`, `tables` и `errors`. Findings имеют стабильные коды, координаты колонок, исходное определение PG-типа, ожидаемое и фактическое CH-представление. Одна колонка может иметь несколько findings, например по типу и nullable. JSON включает совпадения и геометаданные; text показывает различия и итог по таблицам. Порядок результатов детерминирован. Диагностика идёт в stderr, отчёт — в stdout; сообщения драйверов с потенциальными секретами не выводятся.

Текущий `report_version = 2`: у таблицы `source` содержит найденный источник или `null`, если источник отсутствует, неоднозначен либо не прочитан. `source_candidates` перечисляет найденные PG-отношения. В версии 1 поле `source` всегда было объектом, включая предполагаемое имя отсутствующей таблицы. Таблицы в отчёте сортируются по CH-базе и имени.

`complete=false` означает, что полную проверку подтвердить нельзя: есть неподдержанный тип/объект, ошибка чтения или наблюдаемое изменение схемы. Во всех этих случаях возвращаются `status=unknown` и exit 2; успешно найденные отличия сохраняются.

## Правила сравнения

Полная [матрица совместимости](docs/compatibility.md) описывает типы и ограничения. Поддержаны целые, float, Bool, String, UUID, ограниченный numeric/Decimal, date/timestamp, строковые представления JSON/bytea и геометрии.

- Безопасное расширение назначения разрешено; сужение и signed → unsigned дают mismatch.
- Decimal проверяется по исходным precision/scale PG, включая ёмкость целой части.
- Nullable проверяется независимо от базового типа. DEFAULT не заменяет возможность хранить NULL.
- Все семь встроенных геотипов PG и PostGIS `geometry`/`geography` сопоставляются со String. PostGIS определяется по принадлежности типа расширению. В JSON сохраняются объявленные subtype, SRID и Z/M; CH String эти ограничения не выражает.
- Лишняя CH-колонка даёт mismatch. Исключения перечисляются в `comparison.ignore_target_columns` и применяются ко всем найденным таблицам; автоматического исключения служебных полей PeerDB нет. Исключение не может скрыть колонку PG.
- Порядок колонок игнорируется. Переименование отображается как отсутствующая и лишняя колонки. ALIAS/MATERIALIZED вместо исходной колонки дают mismatch.
- Domains, enum, массивы, unconstrained numeric, generated columns и прочие неподдержанные случаи дают unknown.

Проверяются постоянные обычные PG-таблицы и родительские partitioned tables, в CH — распознанные движки семейства MergeTree. Разделы PG не обходятся. Не сравниваются ограничения, индексы, выражения DEFAULT, ключи сортировки, TTL и codecs.

Приложение читает каталоги дважды и сообщает `schema_changed_during_check`, если прочитанные метаданные изменились. Изменение набора PG-кандидатов даёт `source_mapping_changed_during_check`, а добавление или удаление CH-таблицы — `table_set_changed_during_check`. Все три случая дают exit 2 с сохранением уже найденных различий. Это не атомарный снимок двух серверов и не защита от DDL после завершения проверки.

Проверка схемы не проверяет строки и сериализацию CDC. Диапазоны дат, специальные числовые значения и формат String остаются частью соглашения с транспортом. Произвольные CHECK-ограничения PostGIS и история изменений SRID не восстанавливаются.

## Права

PostgreSQL: подключение к базе, чтение `pg_catalog` и выполнение стандартных функций метаданных; для PostGIS — доступ к namespace расширения и его typmod-функциям. Стандартный LOGIN-пользователь с обычными PUBLIC-привилегиями на каталоги подходит. Клиент дополнительно включает `default_transaction_read_only=on`.

ClickHouse: `SHOW TABLES, SHOW COLUMNS ON *.*`. Глобальная видимость метаданных нужна, чтобы отличать отсутствующую таблицу от скрытой из-за прав. При её отсутствии приложение возвращает exit 2. Клиент устанавливает `readonly=2`.

SELECT пользовательских строк и права DDL/DML для приложения не нужны. Интеграционные тесты проверяют запуск с ограниченными пользователями и отказ в чтении строк/ALTER TABLE.

## Разработка и CI

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --lib --bins
```

[GitHub Actions](.github/workflows/ci.yml) запускает Docker service containers: PostgreSQL 14.24 и PostgreSQL 14.24 + PostGIS 3.6.4, каждый с ClickHouse 26.8.36.3 LTS. Образы закреплены digest в workflow; Rust для CI — 1.99.1. Testcontainers и Compose не используются.

Локальный запуск той же матрицы требует самостоятельно поднятых тестовых серверов и административных подключений:

```sh
PG_URL='postgres://postgres:test-pass@localhost:5432/postgres?sslmode=disable' \
CH_URL='http://localhost:8123' CH_USER=default CH_PASSWORD=test-pass \
TEST_POSTGIS=false TEST_ARTIFACTS=test-results \
cargo test --locked --features integration --test integration -- --nocapture
```

Для сервера с PostGIS задайте `TEST_POSTGIS=true`. Отсутствие endpoints — ошибка теста. Harness создаёт отдельные базы и пользователей, применяет независимые SQL-fixtures и удаляет их после успешного прогона. Используйте отдельные тестовые серверы.

TLS-тест запускается отдельно после настройки сервисов скриптом `tests/fixtures/configure-tls.sh`, как в workflow. Он проверяет доверенный CA, недоверенный сертификат и несовпадение hostname у обеих СУБД. В обычном локальном прогоне этот тест помечен ignored; в CI его явный запуск обязателен.

Код разделён на конфигурацию, адаптеры каталогов, чистое ядро сравнения и отчёт. В бинарнике нет команды миграции или SQL-планировщика изменений.
