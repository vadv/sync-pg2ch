# Матрица совместимости v1

Статус: правила реализованы в первой версии CLI и покрыты unit-тестами и независимыми Docker-fixtures. Политика `pg-clickhouse-v1` допускает проверенные расширения типов ClickHouse. Полный контракт результата находится в [плане](../PLAN.md).

## Область проверки

Сначала определяется ожидаемое представление PG-типа в CH, затем проверяется фактический CH-тип. Исходные ограничения PG сохраняются отдельно от канонического CH-представления: совместимость проверяется по требованиям источника. Проверка направленная: допустимость PG → CH не означает допустимость обратного преобразования.

`match` означает совпадение после нормализации, `compatible` означает разрешённое отличие, `mismatch` означает известное нарушение правила, `unknown` означает отсутствие правила. Неизвестное не считается совместимым. Сочетание результатов выбирает худший: unknown, mismatch, compatible, match.

Предлагаемые отображения date, timestamp, json/jsonb, bytea и геотипов являются соглашениями о представлении. Схема не доказывает корректность сериализатора, отсутствие специальных значений или попадание всех значений PG в диапазон CH. Приложение выводит эти ограничения в метаданных политики отчёта.

## Базовые отображения

Nullable сравнивается отдельно от базового типа. Serial/identity используют базовый integer-тип.

| PostgreSQL 14 | Базовое представление CH | Допустимые расширения в v1 |
|---|---|---|
| smallint | Int16 | Int32, Int64 |
| integer | Int32 | Int64 |
| bigint | Int64 | Нет дополнительных правил v1 |
| real | Float32 | Float64 |
| double precision | Float64 | Нет |
| boolean | Bool | Только проверенные синтаксические алиасы Bool |
| text, varchar(n), varchar | String | LowCardinality(String) как допустимая обёртка |
| uuid | UUID | Нет |
| numeric(p,s) | Decimal(p,s), где 1 ≤ p ≤ 76 и 0 ≤ s ≤ p | По правилу ёмкости Decimal |
| date | Date32 | Нет; Date считается более узким представлением |
| timestamp(p) without time zone | DateTime64(p), без явно заданной timezone | Увеличение p в пределах проверенного диапазона |
| timestamp(p) with time zone | DateTime64(p, 'UTC') | Увеличение p при той же timezone и проверенном диапазоне |
| json, jsonb | String | LowCardinality(String); только соглашение о текстовом JSON |
| tsvector | String | LowCardinality(String); текстовое представление tsvector |
| bytea | String | Только соглашение о передаче байтов |
| point, line, lseg, box, path, polygon, circle | String | LowCardinality(String); текстовое представление геометрии |
| PostGIS geometry, geography, включая typmod | String | LowCardinality(String); текстовое представление геометрии |

У timestamp без явного precision используется precision 6. Для `timestamptz` назначение `DateTime64(p)` без явной зоны совместимо, если фактическая `timezone()` подключения CH — `UTC` или `Etc/UTC`, а точность подходит. Результат имеет код `inherited_timezone`; прочитанная зона сохраняется в `default_timezone`. Другая зона даёт `timezone_mismatch`, неизвестная — `implicit_timezone_unknown`. Явная не-UTC зона остаётся несовместимой.

Настройка [session_timezone](https://clickhouse.com/docs/reference/settings/session-settings/other#session_timezone) может переопределить серверную зону, поэтому проверяется зона текущего подключения. Совместимость предполагает ту же настройку у CDC. Для timestamp without time zone схема не доказывает правильную интерпретацию локального времени.

У varchar(n) ограничение длины не переносится в CH; String является выбранным представлением. Char(n), time, interval, unconstrained numeric, numeric вне поддержанного p/s, массивы, domains, enum, composite/range и типы расширений без отдельного правила дают `unknown` в v1. PostGIS geometry/geography имеют отдельные правила и не попадают в общий отказ для расширений. Типы CH вне поддержанного парсера и матрицы также дают `unknown`, без fallback в String.

## Геотипы

В v1 включены два семейства источников:

- Встроенные типы PG: point, line, lseg, box, path, polygon и circle. Для них не нужен PostGIS; это двумерные геометрические объекты.
- PostGIS geometry/geography, в том числе geometry(Point,4326), geometry(Polygon,3857), geography(Point,4326), MultiPoint, LineString, MultiLineString, MultiPolygon и GeometryCollection, где подтип допустим для данного семейства.

Базовое правило обоих семейств: сериализованная геометрия → CH String, nullable по общей матрице. LowCardinality(String) допускается как строковая обёртка. Это явное правило для распознанных типов, а не автоматическое приведение любого extension-типа в String.

Основание выбора String: [документация PeerDB](https://docs.peerdb.io/datatypes/datatype-matrix) указывает это отображение для geometry/geography. В [интеграционном тесте PeerDB для PG geo-типов](https://github.com/PeerDB-io/peerdb/blob/8a3660f976c0845bff163ea123581e5cbdd8a5d7/flow/e2e/clickhouse_suite.go#L2706) point, line, lseg, box, path, polygon и circle проверяются как строки в CH. Это исследование конкретного upstream commit, а не проверка установленной версии пользователя.

Формат строки зависит от источника: тест PeerDB ожидает WKT для point, но для polygon и нескольких остальных встроенных типов ожидает текстовую запись PostgreSQL. Поэтому приложение не требует, чтобы любая геострока была WKT. В [коде обработки PostGIS PeerDB](https://github.com/PeerDB-io/peerdb/blob/8a3660f976c0845bff163ea123581e5cbdd8a5d7/flow/shared/datatypes/geo.go) используется WKT с префиксом SRID при его наличии. Валидация этих значений остаётся за транспортом, приложение проверяет схему.

Типы pg_catalog.point, pg_catalog.polygon, PostGIS geometry(Point,...) и geometry(Polygon,...) сохраняются раздельно. Для PostGIS отчёт содержит семейство, подтип, SRID и размерность/флаги Z/M, если они объявлены. OID разрешается через каталоги текущей базы; schema и версия extension сохраняются. Тип с именем geometry, не принадлежащий PostGIS, не получает геоправило.

У CH String нет полей SRID, subtype и размерности. Они выводятся как сведения источника с отметкой `not_represented_in_target_schema`; их отсутствие в String не является ошибкой. Для unconstrained geometry параметры не угадываются по строкам. Ограничения, заданные только произвольными CHECK вместо typmod, не выводятся автоматически в v1; это ограничение метаданных явно отмечается и не мешает правилу geometry → String.

Из этого следует предел diff: изменение geometry(Point,4326) в geometry(Point,3857) при неизменном CH String нельзя распознать как историческое изменение без предыдущего снимка или отдельного контракта SRID. V1 показывает текущие исходные метаданные и не обещает контроль истории SRID.

Нативные CH Point/Ring/Polygon/MultiPolygon/Geometry не считаются расширениями String. CH Point представлен Tuple(Float64, Float64), Ring является массивом точек, Polygon является массивом колец с возможными отверстиями. Поэтому одинаковые слова point/polygon между СУБД не доказывают совместимость. Эти CH geo-типы распознаются для диагностики; при строковом правиле возвращается `mismatch` с кодом `geo_representation_mismatch`. Поддержка нативного geo-представления требует отдельного правила преобразования; она не включается по совпадению имён.

Геокейсы проверяют метаданные и выбор строкового правила, не повторяют реализацию CDC. Пространственные функции, преобразование координат и библиотеки GEOS/PROJ в бинарник не добавляются.

## Правила расширений

Целые сравниваются по полному диапазону базового PG-типа. Int16 → Int32/Int64 и Int32 → Int64 допустимы. Переход signed → unsigned несовместим: приложение не предполагает неотрицательность существующих значений и не анализирует CHECK. Известное сужение даёт `mismatch`. Более широкие integer-типы CH вне матрицы v1 дают `unknown` до добавления тестов.

Float32 → Float64 допустим. Float64 → Float32 несовместим. Переходы между integer, float, Decimal и String не разрешаются автоматически, даже если для части значений преобразование возможно. Известные пары вне разрешённых семейств дают `mismatch`; неподдержанные типы дают `unknown`.

Для numeric(Ps,Ss) → Decimal(Pt,St) требуются оба условия:

```text
St >= Ss
Pt - St >= Ps - Ss
```

Увеличение scale без достаточной ёмкости целой части несовместимо. Например, numeric(18,2) → Decimal(18,4) не проходит, а numeric(18,2) → Decimal(38,4) проходит. Эти условия относятся к конечным значениям; PG numeric допускает NaN, который не покрывается гарантией.

Для CH используются фактические параметры из метаданных. Алиасы Decimal32/64/128/256 нормализуются с учётом их ёмкости 9/18/38/76 цифр. Ожидаемое CH-представление тоже канонизируется для вывода и определения `match`: сервер может не сохранять исходное написание precision из CREATE TABLE. При проверке ёмкости Ps/Ss берутся из PG, без округления до ёмкости CH-типа. Например, numeric(10,2) → Decimal64(4) совместим: источнику нужны 8 целых цифр, назначение вмещает 14. Docker-кейсы проверяют канонизацию и не требуют восстановления потерянного текста DDL.

У DateTime64 увеличение precision допустимо только при сохранении диапазона базового представления. В v1 разрешено p_source ≤ p_target ≤ 6 на закреплённом LTS. Precision 7–9 возвращает `unknown` до добавления отдельного правила; уменьшение precision возвращает `mismatch`. Несовпадение timezone является отдельным `mismatch`. DateTime и DateTime64 не сводятся друг к другу автоматически.

LowCardinality разрешается как обёртка только для перечисленных строковых представлений. Она сохраняется в отчёте и даёт `compatible`; вложенный Nullable обрабатывается отдельно. Неизвестные вложенные конструкции не удаляются при нормализации.

## Nullable и записываемые колонки

Отсутствие NULL в текущих данных не влияет на результат: пользовательские строки не читаются.

| PG | CH | Результат при подходящем базовом типе |
|---|---|---|
| NOT NULL | T | match либо compatible по типу |
| NOT NULL | Nullable(T) | compatible |
| nullable | Nullable(T) | match либо compatible по типу |
| nullable | T | mismatch |

DEFAULT на стороне CH не компенсирует запрет NULL. Выражения defaults между СУБД не сравниваются: предполагается передача значений исходных колонок. Колонка ALIAS/MATERIALIZED на месте обычной исходной колонки даёт `column_not_writable`, даже если базовый тип совпал.

Дополнительная колонка CH даёт `compatible` с кодом `target_only_column` независимо от DEFAULT/Nullable/MATERIALIZED, пока она не перечислена в `ignore_target_columns`. Колонка сохраняется при `apply`; предполагается запись CDC по именам колонок. Отсутствующая исходная колонка даёт `source_column_missing_in_target`.

## Начальные тестовые случаи

К каждому случаю добавляются независимые PG/CH DDL-fixtures. Если nullable не оговорена, источник имеет NOT NULL, назначение использует тип без Nullable. Для структурных случаев типы остальных колонок совпадают. Коды возврата указаны для единственного случая без других ошибок.

| Случай | Ожидаемый результат | Exit |
|---|---|---|
| integer NOT NULL → Int32 | match | 0 |
| integer NOT NULL → Int64 | compatible | 0 |
| bigint NOT NULL → Int32 | mismatch | 1 |
| bigint NOT NULL → UInt64 | mismatch | 1 |
| real NOT NULL → Float64 | compatible | 0 |
| double precision NOT NULL → Float32 | mismatch | 1 |
| numeric(18,2) NOT NULL → Decimal(38,4) | compatible | 0 |
| numeric(10,2) NOT NULL → Decimal64(4) | compatible: учитываются исходные 8 целых цифр PG | 0 |
| numeric(18,2) NOT NULL → Decimal(18,4) | mismatch: недостаточно целых цифр | 1 |
| numeric(18,4) NOT NULL → Decimal(38,2) | mismatch: потеря scale | 1 |
| numeric без p/s → Decimal(76,38) | unknown | 2 |
| integer NOT NULL → Nullable(Int32) | compatible | 0 |
| integer nullable → Int64 | mismatch: NULL не представим | 1 |
| text NOT NULL → LowCardinality(String) | compatible | 0 |
| timestamp(3) → DateTime64(6), одинаковая nullable | compatible после проверки диапазона LTS | 0 |
| timestamptz(6) → DateTime64(3, 'UTC') | mismatch | 1 |
| timestamptz(6) → DateTime64(9, 'UTC') | unknown в v1 | 2 |
| timestamptz(6) → DateTime64(6, 'Europe/Berlin') | mismatch: timezone | 1 |
| Одинаковые колонки в другом порядке | match | 0 |
| Лишняя служебная CH-колонка без исключения | compatible | 0 |
| Та же колонка с явным исключением | match; исключение отражено в отчёте | 0 |
| Неподдержанный PG array/domain/enum | unknown | 2 |
| PG point/polygon NOT NULL → String | match по геоправилу | 0 |
| Остальные встроенные PG geo-типы → String | match по геоправилу | 0 |
| geometry(Point,4326) NOT NULL → String | match; subtype/SRID доступны в отчёте | 0 |
| geometry(Polygon,3857) nullable → Nullable(String) | match | 0 |
| geography(Point,4326) NOT NULL → LowCardinality(String) | compatible | 0 |
| geometry(PointZ,4326) / PointM / PointZM → String | match; объявленные измерения сохранены в отчёте | 0 |
| geometry без typmod → String | match; ограничения subtype/SRID не угадываются | 0 |
| geometry nullable → String | mismatch: NULL не представим | 1 |
| PG point → CH Point при строковом геоправиле | mismatch: требуется другое представление | 1 |
| PG polygon → CH Polygon при строковом геоправиле | mismatch: требуется другое представление | 1 |
| PostGIS в отдельной schema → String | match; нет зависимости от public/search_path | 0 |
| Пользовательский тип geometry без принадлежности PostGIS | unknown | 2 |
| Одна пара совпала, вторая недоступна | неполный отчёт, результат первой сохранён | 2 |

Для каждого поддержанного базового отображения обязательны match-кейс, nullable-кейсы и хотя бы один несовместимый тип. Для каждого разрешённого widening обязательны прямое и обратное направления. Матрица расширяется вместе с правилом; поддержка не объявляется по результату одного успешного INSERT.

## Источники и границы уверенности

Правила расширения являются решениями приложения. Свойства типов проверяются по [PG 14 numeric](https://www.postgresql.org/docs/14/datatype-numeric.html), [PG 14 date/time](https://www.postgresql.org/docs/14/datatype-datetime.html), [CH Decimal](https://clickhouse.com/docs/reference/data-types/decimal), [CH DateTime64](https://clickhouse.com/docs/reference/data-types/datetime64) и [CH Nullable](https://clickhouse.com/docs/reference/data-types/nullable). Документация CH может описывать более свежие возможности, чем выбранный образ, поэтому его поведение подтверждается Docker-тестами.

Источники по geo: [встроенные PG 14 geometric types](https://www.postgresql.org/docs/14/datatype-geometric.html), [модель типов PostGIS](https://postgis.net/docs/using_postgis_dbmanagement.html), [CH geometric types](https://clickhouse.com/docs/reference/data-types/geo), [принадлежность extension через pg_depend](https://www.postgresql.org/docs/14/catalog-pg-depend.html).

Сравнение схем не проверяет специальные числовые значения, бесконечные даты, фактические диапазоны значений и историю событий. Неполная поддержка типа не подменяется успешным результатом. Подтверждённая совместимость относится к описанной политике, а не к протоколу конкретного CDC.
