//! LDBC SNB Interactive query shapes through the embedded database (§17's
//! SNB gate; fgdb-2ht8c, fgdb-0gl5x). In-house paraphrases of the IC, IS and
//! IU queries, with literal parameters, run against a four-person SNB-shaped
//! graph. Each answer is derived by hand from that graph. This locks in the
//! query surface the workload needs; it is not an SNB compliance run, and it
//! measures no throughput.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    GraphWriteProgramPolicy, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const LABELS: [&str; 13] = [
    "Person",
    "City",
    "Country",
    "Place",
    "University",
    "Company",
    "Organisation",
    "TagClass",
    "Tag",
    "Forum",
    "Post",
    "Comment",
    "Message",
];
const RELATIONS: [&str; 15] = [
    "KNOWS",
    "IS_LOCATED_IN",
    "IS_PART_OF",
    "STUDY_AT",
    "WORK_AT",
    "HAS_TYPE",
    "IS_SUBCLASS_OF",
    "HAS_MODERATOR",
    "HAS_MEMBER",
    "HAS_TAG",
    "CONTAINER_OF",
    "HAS_CREATOR",
    "REPLY_OF",
    "LIKES",
    "HAS_INTEREST",
];
const PROPERTIES: [&str; 19] = [
    "id",
    "name",
    "firstName",
    "lastName",
    "birthday",
    "creationDate",
    "gender",
    "browserUsed",
    "locationIP",
    "email",
    "speaks",
    "classYear",
    "workFrom",
    "title",
    "joinDate",
    "content",
    "length",
    "language",
    "imageFile",
];

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    let id = |names: &[&str]| {
        names
            .iter()
            .position(|known| *known == name)
            .map(|at| u64::try_from(at + 1).unwrap())
    };
    match kind {
        GraphSymbolKind::Label => id(&LABELS).map(|at| GraphSymbol::Label(LabelId(at))),
        GraphSymbolKind::Relation => id(&RELATIONS).map(|at| GraphSymbol::Relation(RelationId(at))),
        GraphSymbolKind::Property => {
            id(&PROPERTIES).map(|at| GraphSymbol::Property(PropertyKeyId(at)))
        }
    }
}

/// The test catalog: names to ids, and the whole reverse catalog, so
/// `type(r)` names a relationship type the query text does not mention.
struct Catalog;

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        for (at, name) in (1..).zip(LABELS) {
            catalog.insert_label(LabelId(at), name);
        }
        for (at, name) in (1..).zip(RELATIONS) {
            catalog.insert_relation(RelationId(at), name);
        }
        for (at, name) in (1..).zip(PROPERTIES) {
            catalog.insert_property(PropertyKeyId(at), name);
        }
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5b; 32],
        DatabaseSecurityNamespaceId([0x5c; 32]),
        [0x5d; 32],
    )
}

fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 1_000, 1_000, 1_000)
}

/// The probe graph: four people in a KNOWS chain 1-2-3-4, two cities in two
/// countries, a university, a company, two tags under a two-level tag class
/// hierarchy, one forum, two posts and one comment, and likes.
const SETUP: [&str; 6] = [
    "CREATE (c1:City:Place {id: 1, name: 'Berlin'}), (c2:City:Place {id: 2, name: 'Paris'}),
       (co1:Country:Place {id: 10, name: 'Germany'}), (co2:Country:Place {id: 11, name: 'France'}),
       (c1)-[:IS_PART_OF]->(co1), (c2)-[:IS_PART_OF]->(co2)",
    "CREATE (p1:Person {id: 1, firstName: 'Ann', lastName: 'Abel', birthday: 100, creationDate: 1000,
         gender: 'female', browserUsed: 'Firefox', locationIP: '1.1.1.1', email: ['ann@x.org'], speaks: ['en']}),
       (p2:Person {id: 2, firstName: 'Bob', lastName: 'Bauer', birthday: 200, creationDate: 1001,
         gender: 'male', browserUsed: 'Chrome', locationIP: '2.2.2.2', email: ['bob@x.org'], speaks: ['de']}),
       (p3:Person {id: 3, firstName: 'Ann', lastName: 'Cole', birthday: 300, creationDate: 1002,
         gender: 'female', browserUsed: 'Safari', locationIP: '3.3.3.3', email: [], speaks: ['fr', 'en']}),
       (p4:Person {id: 4, firstName: 'Dan', lastName: 'Diaz', birthday: 400, creationDate: 1003,
         gender: 'male', browserUsed: 'Chrome', locationIP: '4.4.4.4', email: [], speaks: ['es']}),
       (p1)-[:KNOWS {creationDate: 5}]->(p2), (p2)-[:KNOWS {creationDate: 6}]->(p3),
       (p3)-[:KNOWS {creationDate: 7}]->(p4)",
    "MATCH (p:Person), (c:City) WHERE (p.id IN [1, 3] AND c.id = 1) OR (p.id IN [2, 4] AND c.id = 2)
       CREATE (p)-[:IS_LOCATED_IN]->(c)",
    "CREATE (u:University:Organisation {id: 20, name: 'TU Berlin'}),
       (comp:Company:Organisation {id: 21, name: 'Acme'}),
       (tc:TagClass {id: 40, name: 'Thing'}), (tc2:TagClass {id: 41, name: 'Person'}),
       (tc2)-[:IS_SUBCLASS_OF]->(tc),
       (t1:Tag {id: 30, name: 'Bach'}), (t2:Tag {id: 31, name: 'Rust'}),
       (t1)-[:HAS_TYPE]->(tc2), (t2)-[:HAS_TYPE]->(tc)",
    "MATCH (u:University {id: 20}), (comp:Company {id: 21}), (c1:City {id: 1}), (co1:Country {id: 10}),
             (p3:Person {id: 3}), (p4:Person {id: 4})
       CREATE (u)-[:IS_LOCATED_IN]->(c1), (comp)-[:IS_LOCATED_IN]->(co1),
              (p3)-[:STUDY_AT {classYear: 2005}]->(u), (p3)-[:WORK_AT {workFrom: 2008}]->(comp),
              (p4)-[:WORK_AT {workFrom: 2011}]->(comp)",
    "MATCH (p1:Person {id: 1}), (p2:Person {id: 2}), (p3:Person {id: 3}), (t1:Tag {id: 30}),
             (t2:Tag {id: 31}), (co1:Country {id: 10}), (co2:Country {id: 11})
       CREATE (f:Forum {id: 50, title: 'Group for Bach', creationDate: 900}),
              (f)-[:HAS_MODERATOR]->(p1), (f)-[:HAS_MEMBER {joinDate: 950}]->(p2),
              (f)-[:HAS_MEMBER {joinDate: 960}]->(p3), (f)-[:HAS_TAG]->(t1),
              (po:Post:Message {id: 60, creationDate: 1100, content: 'hello', length: 5, language: 'en',
                browserUsed: 'Chrome', locationIP: '2.2.2.2'}),
              (po)-[:HAS_CREATOR]->(p2), (f)-[:CONTAINER_OF]->(po), (po)-[:HAS_TAG]->(t1),
              (po)-[:HAS_TAG]->(t2), (po)-[:IS_LOCATED_IN]->(co1),
              (po2:Post:Message {id: 62, creationDate: 1150, imageFile: 'photo.png', length: 0,
                browserUsed: 'Safari', locationIP: '3.3.3.3'}),
              (po2)-[:HAS_CREATOR]->(p3), (f)-[:CONTAINER_OF]->(po2), (po2)-[:HAS_TAG]->(t1),
              (po2)-[:IS_LOCATED_IN]->(co2),
              (cm:Comment:Message {id: 61, creationDate: 1200, content: 'nice', length: 4,
                browserUsed: 'Safari', locationIP: '3.3.3.3'}),
              (cm)-[:HAS_CREATOR]->(p3), (cm)-[:REPLY_OF]->(po), (cm)-[:HAS_TAG]->(t1),
              (cm)-[:IS_LOCATED_IN]->(co2),
              (p1)-[:LIKES {creationDate: 1300}]->(po), (p3)-[:LIKES {creationDate: 1310}]->(po),
              (p1)-[:LIKES {creationDate: 1320}]->(cm), (p1)-[:HAS_INTEREST]->(t1)",
];

/// One cell as text: integers and counts as numbers, text quoted, lists
/// bracketed, NULL as `null`.
fn cell(value: &QueryValue) -> String {
    fn graph(value: &GraphValue) -> String {
        match value {
            GraphValue::Scalar(CanonicalScalar::Null) => "null".into(),
            GraphValue::Scalar(CanonicalScalar::Int(n)) => n.to_string(),
            GraphValue::Scalar(CanonicalScalar::Bool(b)) => b.to_string(),
            GraphValue::Scalar(CanonicalScalar::Text(text)) => format!("'{}'", text.as_str()),
            GraphValue::List(items) => format!(
                "[{}]",
                items.iter().map(graph).collect::<Vec<_>>().join(", ")
            ),
            other => panic!("unexpected cell {other:?}"),
        }
    }
    match value {
        QueryValue::Value(value) => graph(value),
        QueryValue::Count(n) => n.to_string(),
        QueryValue::Integer(n) => n.to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn rendered(result: QueryResult) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("a read answers rows")
    };
    rows.iter()
        .map(|row| row.iter().map(cell).collect())
        .collect()
}

/// The hand-derived answer of each read shape, row by row in its ORDER BY.
#[allow(clippy::type_complexity)]
fn reads() -> Vec<(&'static str, &'static str, Vec<Vec<&'static str>>)> {
    vec![
        (
            "IC1",
            "MATCH (p:Person {id: 1}), (friend:Person {firstName: 'Ann'})
             WHERE NOT p = friend
             WITH p, friend
             MATCH path = shortestPath((p)-[:KNOWS*1..3]-(friend))
             WITH min(length(path)) AS distance, friend
             ORDER BY distance ASC, friend.lastName ASC, toInteger(friend.id) ASC
             LIMIT 20
             MATCH (friend)-[:IS_LOCATED_IN]->(friendCity:City)
             OPTIONAL MATCH (friend)-[studyAt:STUDY_AT]->(uni:University)-[:IS_LOCATED_IN]->(uniCity:City)
             WITH friend, collect(CASE WHEN uni.name IS NULL THEN null ELSE [uni.name, studyAt.classYear, uniCity.name] END) AS unis, friendCity, distance
             OPTIONAL MATCH (friend)-[workAt:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(companyCountry:Country)
             WITH friend, collect(CASE WHEN company.name IS NULL THEN null ELSE [company.name, workAt.workFrom, companyCountry.name] END) AS companies, unis, friendCity, distance
             RETURN friend.id AS friendId, friend.lastName AS friendLastName, distance AS distanceFromPerson,
               friend.birthday AS friendBirthday, friend.creationDate AS friendCreationDate,
               friend.gender AS friendGender, friend.browserUsed AS friendBrowserUsed,
               friend.locationIP AS friendLocationIp, friend.email AS friendEmails,
               friend.speaks AS friendLanguages, friendCity.name AS friendCityName,
               unis AS friendUniversities, companies AS friendCompanies
             ORDER BY distanceFromPerson ASC, friendLastName ASC, toInteger(friendId) ASC
             LIMIT 20",
            // Ann Cole is two KNOWS hops from person 1; person 1 itself is not
            // a friend.
            vec![vec![
                "3",
                "'Cole'",
                "2",
                "300",
                "1002",
                "'female'",
                "'Safari'",
                "'3.3.3.3'",
                "[]",
                "['fr', 'en']",
                "'Berlin'",
                "[['TU Berlin', 2005, 'Berlin']]",
                "[['Acme', 2008, 'Germany']]",
            ]],
        ),
        (
            "IC2",
            "MATCH (:Person {id: 1})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(message:Message)
             WHERE message.creationDate <= 2000
             RETURN friend.id AS personId, friend.firstName AS personFirstName,
               friend.lastName AS personLastName, message.id AS postOrCommentId,
               coalesce(message.content, message.imageFile) AS postOrCommentContent,
               message.creationDate AS postOrCommentCreationDate
             ORDER BY postOrCommentCreationDate DESC, toInteger(postOrCommentId) ASC
             LIMIT 20",
            vec![vec!["2", "'Bob'", "'Bauer'", "60", "'hello'", "1100"]],
        ),
        (
            "IC4",
            "MATCH (person:Person {id: 1})-[:KNOWS]-(friend:Person),
               (friend)<-[:HAS_CREATOR]-(post:Post)-[:HAS_TAG]->(tag)
             WITH DISTINCT tag, post
             WITH tag,
               CASE WHEN 2000 > post.creationDate >= 1000 THEN 1 ELSE 0 END AS valid,
               CASE WHEN 1000 > post.creationDate THEN 1 ELSE 0 END AS inValid
             WITH tag, sum(valid) AS postCount, sum(inValid) AS inValidPostCount
             WHERE postCount > 0 AND inValidPostCount = 0
             RETURN tag.name AS tagName, postCount
             ORDER BY postCount DESC, tagName ASC
             LIMIT 10",
            vec![vec!["'Bach'", "1"], vec!["'Rust'", "1"]],
        ),
        (
            "IC5",
            "MATCH (person:Person {id: 1})-[:KNOWS*1..2]-(friend)
             WHERE NOT person = friend
             WITH DISTINCT friend
             MATCH (friend)<-[membership:HAS_MEMBER]-(forum)
             WHERE membership.joinDate > 500
             WITH forum, collect(friend) AS friends
             OPTIONAL MATCH (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum)
             WHERE friend IN friends
             WITH forum, count(post) AS postCount
             RETURN forum.title AS forumName, postCount
             ORDER BY postCount DESC, forum.id ASC
             LIMIT 20",
            // Persons 2 and 3 are members of forum 50, which holds one post
            // by each.
            vec![vec!["'Group for Bach'", "2"]],
        ),
        (
            "IC6",
            "MATCH (knownTag:Tag {name: 'Bach'})
             WITH knownTag.id AS knownTagId
             MATCH (person:Person {id: 1})-[:KNOWS*1..2]-(friend)
             WHERE NOT person = friend
             WITH knownTagId, collect(DISTINCT friend) AS friends
             UNWIND friends AS f
             MATCH (f)<-[:HAS_CREATOR]-(post:Post), (post)-[:HAS_TAG]->(t:Tag {id: knownTagId}),
               (post)-[:HAS_TAG]->(tag:Tag)
             WHERE NOT t = tag
             WITH tag.name AS tagName, count(post) AS postCount
             RETURN tagName, postCount
             ORDER BY postCount DESC, tagName ASC
             LIMIT 10",
            // Only post 60 carries Bach and another tag.
            vec![vec!["'Rust'", "1"]],
        ),
        (
            "IC8",
            "MATCH (start:Person {id: 2})<-[:HAS_CREATOR]-(:Message)<-[:REPLY_OF]-(comment:Comment)
               -[:HAS_CREATOR]->(person:Person)
             RETURN person.id AS personId, person.firstName AS personFirstName,
               person.lastName AS personLastName, comment.creationDate AS commentCreationDate,
               comment.id AS commentId, comment.content AS commentContent
             ORDER BY commentCreationDate DESC, commentId ASC
             LIMIT 20",
            vec![vec!["3", "'Ann'", "'Cole'", "1200", "61", "'nice'"]],
        ),
        (
            "IC9",
            "MATCH (root:Person {id: 1})-[:KNOWS*1..2]-(friend:Person)
             WHERE NOT friend = root
             WITH collect(DISTINCT friend) AS friends
             UNWIND friends AS friend
             MATCH (friend)<-[:HAS_CREATOR]-(message:Message)
             WHERE message.creationDate < 2000
             RETURN friend.id AS personId, friend.firstName AS personFirstName,
               friend.lastName AS personLastName, message.id AS commentOrPostId,
               coalesce(message.content, message.imageFile) AS commentOrPostContent,
               message.creationDate AS commentOrPostCreationDate
             ORDER BY commentOrPostCreationDate DESC, message.id ASC
             LIMIT 20",
            vec![
                vec!["3", "'Ann'", "'Cole'", "61", "'nice'", "1200"],
                vec!["3", "'Ann'", "'Cole'", "62", "'photo.png'", "1150"],
                vec!["2", "'Bob'", "'Bauer'", "60", "'hello'", "1100"],
            ],
        ),
        (
            "IC11",
            "MATCH (person:Person {id: 1})-[:KNOWS*1..2]-(friend:Person)
             WHERE NOT (person = friend)
             WITH DISTINCT friend
             MATCH (friend)-[workAt:WORK_AT]->(company:Company)-[:IS_LOCATED_IN]->(:Country {name: 'Germany'})
             WHERE workAt.workFrom < 2010
             RETURN friend.id AS personId, friend.firstName AS personFirstName,
               friend.lastName AS personLastName, company.name AS organizationName,
               workAt.workFrom AS organizationWorkFromYear
             ORDER BY organizationWorkFromYear ASC, toInteger(personId) ASC, organizationName DESC
             LIMIT 10",
            // Person 4 also works at Acme, but is three hops away.
            vec![vec!["3", "'Ann'", "'Cole'", "'Acme'", "2008"]],
        ),
        (
            // IC3 with countryX = countryY = Germany: Bob, in Paris, is a
            // friend outside both countries' cities, and his post 60 is in
            // Germany, so it counts once for X and once for Y. (France and
            // Germany together cover every city here, so the published
            // parameters would answer no row.)
            "IC3",
            "MATCH (countryX:Country {name: 'Germany'}), (countryY:Country {name: 'Germany'}),
               (person:Person {id: 1})
             WITH person, countryX, countryY
             LIMIT 1
             MATCH (city:City)-[:IS_PART_OF]->(country:Country)
             WHERE country IN [countryX, countryY]
             WITH person, countryX, countryY, collect(city) AS cities
             MATCH (person)-[:KNOWS*1..2]-(friend)-[:IS_LOCATED_IN]->(city)
             WHERE NOT person = friend AND NOT city IN cities
             WITH DISTINCT friend, countryX, countryY
             MATCH (friend)<-[:HAS_CREATOR]-(message), (message)-[:IS_LOCATED_IN]->(country)
             WHERE 2000 > message.creationDate >= 1000 AND country IN [countryX, countryY]
             WITH friend,
               CASE WHEN country = countryX THEN 1 ELSE 0 END AS messageX,
               CASE WHEN country = countryY THEN 1 ELSE 0 END AS messageY
             WITH friend, sum(messageX) AS xCount, sum(messageY) AS yCount
             WHERE xCount > 0 AND yCount > 0
             RETURN friend.id AS friendId, friend.firstName AS friendFirstName,
               friend.lastName AS friendLastName, xCount, yCount, xCount + yCount AS xyCount
             ORDER BY xyCount DESC, friendId ASC
             LIMIT 20",
            vec![vec!["2", "'Bob'", "'Bauer'", "1", "1", "2"]],
        ),
        (
            "IC13",
            "MATCH (person1:Person {id: 1}), (person2:Person {id: 4}),
               path = shortestPath((person1)-[:KNOWS*]-(person2))
             RETURN CASE path IS NULL WHEN true THEN -1 ELSE length(path) END AS shortestPathLength",
            vec![vec!["3"]],
        ),
        (
            "IC14",
            "MATCH (person1:Person {id: 1}), (person2:Person {id: 3}),
               path = allShortestPaths((person1)-[:KNOWS*0..]-(person2))
             RETURN [n IN nodes(path) | n.id] AS personIdsInPath",
            vec![vec!["[1, 2, 3]"]],
        ),
        (
            "IS1",
            "MATCH (n:Person {id: 1})-[:IS_LOCATED_IN]->(p:City)
             RETURN n.firstName AS firstName, n.lastName AS lastName, n.birthday AS birthday,
               n.locationIP AS locationIP, n.browserUsed AS browserUsed, p.id AS cityId,
               n.gender AS gender, n.creationDate AS creationDate",
            vec![vec![
                "'Ann'",
                "'Abel'",
                "100",
                "'1.1.1.1'",
                "'Firefox'",
                "1",
                "'female'",
                "1000",
            ]],
        ),
        (
            "IS2",
            "MATCH (:Person {id: 3})<-[:HAS_CREATOR]-(message)
             WITH message, message.id AS messageId, message.creationDate AS messageCreationDate
             ORDER BY messageCreationDate DESC, messageId ASC
             LIMIT 10
             MATCH (message)-[:REPLY_OF*0..]->(post:Post), (post)-[:HAS_CREATOR]->(person)
             RETURN messageId, coalesce(message.imageFile, message.content) AS messageContent,
               messageCreationDate, post.id AS postId, person.id AS personId,
               person.firstName AS personFirstName, person.lastName AS personLastName
             ORDER BY messageCreationDate DESC, messageId ASC",
            // The comment replies to Bob's post; the post is its own root.
            vec![
                vec!["61", "'nice'", "1200", "60", "2", "'Bob'", "'Bauer'"],
                vec!["62", "'photo.png'", "1150", "62", "3", "'Ann'", "'Cole'"],
            ],
        ),
        (
            "IS3",
            "MATCH (n:Person {id: 2})-[r:KNOWS]-(friend)
             RETURN friend.id AS personId, friend.firstName AS firstName,
               friend.lastName AS lastName, r.creationDate AS friendshipCreationDate
             ORDER BY friendshipCreationDate DESC, toInteger(personId) ASC",
            vec![
                vec!["3", "'Ann'", "'Cole'", "6"],
                vec!["1", "'Ann'", "'Abel'", "5"],
            ],
        ),
        (
            "IS4",
            "MATCH (m:Message {id: 62})
             RETURN m.creationDate AS messageCreationDate,
               coalesce(m.content, m.imageFile) AS messageContent",
            vec![vec!["1150", "'photo.png'"]],
        ),
        (
            "IS5",
            "MATCH (m:Message {id: 61})-[:HAS_CREATOR]->(p:Person)
             RETURN p.id AS personId, p.firstName AS firstName, p.lastName AS lastName",
            vec![vec!["3", "'Ann'", "'Cole'"]],
        ),
        (
            "IS6",
            "MATCH (m:Message {id: 61})-[:REPLY_OF*0..]->(p:Post)<-[:CONTAINER_OF]-(f:Forum)
               -[:HAS_MODERATOR]->(mod:Person)
             RETURN f.id AS forumId, f.title AS forumTitle, mod.id AS moderatorId,
               mod.firstName AS moderatorFirstName, mod.lastName AS moderatorLastName",
            vec![vec!["50", "'Group for Bach'", "1", "'Ann'", "'Abel'"]],
        ),
    ]
}

#[test]
fn snb_interactive_shapes_answer_from_the_graph() {
    let ((), report) = run_async_under_lab(0x5b1b_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut next = 1_000u128;
        for text in SETUP {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                Catalog,
                RelationId(1),
                write_policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{text}: {error}"));
        }
        for (name, text, expected) in reads() {
            let answer = db
                .query(&cx, text, &GqlParameters::new(), Catalog, query_policy())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(rendered(answer), expected, "{name}");
        }
        // IC12: `collect(DISTINCT tag.name)` follows the plan's row order, so
        // its members compare as a set. Both tags reach the class 'Thing'
        // (Rust directly, Bach through 'Person'), and Ann Cole's one comment
        // replies to Bob's post that carries both.
        let ic12 = db
            .query(
                &cx,
                "MATCH (tag:Tag)-[:HAS_TYPE]->(:TagClass)-[:IS_SUBCLASS_OF*0..]->(baseTagClass:TagClass)
                 WHERE tag.name = 'Thing' OR baseTagClass.name = 'Thing'
                 WITH collect(tag.id) AS tags
                 MATCH (:Person {id: 2})-[:KNOWS]-(friend:Person)<-[:HAS_CREATOR]-(comment:Comment)
                   -[:REPLY_OF]->(:Post)-[:HAS_TAG]->(tag:Tag)
                 WHERE tag.id IN tags
                 RETURN friend.id AS personId, friend.firstName AS personFirstName,
                   friend.lastName AS personLastName, collect(DISTINCT tag.name) AS tagNames,
                   count(DISTINCT comment) AS replyCount
                 ORDER BY replyCount DESC, toInteger(personId) ASC
                 LIMIT 20",
                &GqlParameters::new(),
                Catalog,
                query_policy(),
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = ic12 else {
            panic!("IC12 answers rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].iter().map(cell).collect::<Vec<_>>()[..3],
            ["3", "'Ann'", "'Cole'"]
        );
        let QueryValue::Value(GraphValue::List(tags)) = &rows[0][3] else {
            panic!("IC12 tagNames is a list")
        };
        let mut tags: Vec<String> = tags
            .iter()
            .map(|tag| cell(&QueryValue::Value(tag.clone())))
            .collect();
        tags.sort();
        assert_eq!(tags, ["'Bach'", "'Rust'"]);
        assert_eq!(cell(&rows[0][4]), "1");

        // The eight updates, each checked by reading back what it made. IU1,
        // IU4, IU6 and IU7 MATCH between their writes.
        for (name, text, check, expected) in updates() {
            db.query_write(
                &txcx,
                &cx,
                &commit,
                text,
                &GqlParameters::new(),
                Catalog,
                RelationId(1),
                write_policy(),
                |request| {
                    next += 1;
                    Ok::<_, &'static str>(match request.request {
                        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(next)),
                        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(next)),
                    })
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            let answer = db
                .query(&cx, &check, &GqlParameters::new(), Catalog, query_policy())
                .unwrap_or_else(|error| panic!("{name} check: {error}"));
            assert_eq!(rendered(answer), expected, "{name}");
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Each update, a read of what it made, and that read's answer.
#[allow(clippy::type_complexity)]
fn updates() -> Vec<(&'static str, &'static str, String, Vec<Vec<&'static str>>)> {
    let edges_from = "RETURN type(r) AS t, x.id AS id ORDER BY t, id";
    vec![
        (
            // `WITH DISTINCT p` after each UNWIND step, as the LDBC reference
            // writes it: a plain `WITH p` keeps one row per tag, and each
            // would create its own STUDY_AT and WORK_AT relationship.
            "IU1",
            "MATCH (c:City {id: 1})
             CREATE (p:Person {id: 9, firstName: 'Eve', lastName: 'Ernst', gender: 'female',
               birthday: 500, creationDate: 2000, locationIP: '9.9.9.9', browserUsed: 'Firefox',
               speaks: ['en'], email: ['eve@x.org']})-[:IS_LOCATED_IN]->(c)
             WITH p
             UNWIND [30, 31] AS tagId
             MATCH (t:Tag {id: tagId})
             CREATE (p)-[:HAS_INTEREST]->(t)
             WITH DISTINCT p
             UNWIND [[20, 2010]] AS s
             MATCH (u:Organisation {id: s[0]})
             CREATE (p)-[:STUDY_AT {classYear: s[1]}]->(u)
             WITH DISTINCT p
             UNWIND [[21, 2015]] AS w
             MATCH (comp:Organisation {id: w[0]})
             CREATE (p)-[:WORK_AT {workFrom: w[1]}]->(comp)",
            format!("MATCH (:Person {{id: 9}})-[r]->(x) {edges_from}"),
            vec![
                vec!["'HAS_INTEREST'", "30"],
                vec!["'HAS_INTEREST'", "31"],
                vec!["'IS_LOCATED_IN'", "1"],
                vec!["'STUDY_AT'", "20"],
                vec!["'WORK_AT'", "21"],
            ],
        ),
        (
            "IU2",
            "MATCH (person:Person {id: 4}), (post:Post {id: 60})
             CREATE (person)-[:LIKES {creationDate: 2100}]->(post)",
            "MATCH (:Person {id: 4})-[l:LIKES]->(m) RETURN m.id AS id, l.creationDate AS at ORDER BY id"
                .into(),
            vec![vec!["60", "2100"]],
        ),
        (
            "IU3",
            "MATCH (person:Person {id: 4}), (comment:Comment {id: 61})
             CREATE (person)-[:LIKES {creationDate: 2110}]->(comment)",
            "MATCH (:Person {id: 4})-[l:LIKES]->(m) RETURN m.id AS id, l.creationDate AS at ORDER BY id"
                .into(),
            vec![vec!["60", "2100"], vec!["61", "2110"]],
        ),
        (
            "IU4",
            "MATCH (p:Person {id: 2})
             CREATE (f:Forum {id: 51, title: 'Rustaceans', creationDate: 2200})-[:HAS_MODERATOR]->(p)
             WITH f
             UNWIND [31] AS tagId
             MATCH (t:Tag {id: tagId})
             CREATE (f)-[:HAS_TAG]->(t)",
            format!("MATCH (:Forum {{id: 51}})-[r]->(x) {edges_from}"),
            vec![vec!["'HAS_MODERATOR'", "2"], vec!["'HAS_TAG'", "31"]],
        ),
        (
            "IU5",
            "MATCH (f:Forum {id: 50}), (p:Person {id: 4})
             CREATE (f)-[:HAS_MEMBER {joinDate: 2300}]->(p)",
            "MATCH (:Forum {id: 50})-[m:HAS_MEMBER]->(p) RETURN p.id AS id, m.joinDate AS at ORDER BY id"
                .into(),
            vec![vec!["2", "950"], vec!["3", "960"], vec!["4", "2300"]],
        ),
        (
            "IU6",
            "MATCH (author:Person {id: 4}), (country:Country {id: 11}), (forum:Forum {id: 50})
             CREATE (author)<-[:HAS_CREATOR]-(p:Post:Message {id: 63, creationDate: 2400,
               locationIP: '4.4.4.4', browserUsed: 'Chrome', language: 'es',
               content: CASE 'hola' WHEN '' THEN null ELSE 'hola' END,
               imageFile: CASE '' WHEN '' THEN null ELSE '' END, length: 4})<-[:CONTAINER_OF]-(forum),
               (p)-[:IS_LOCATED_IN]->(country)
             WITH p
             UNWIND [30] AS tagId
             MATCH (t:Tag {id: tagId})
             CREATE (p)-[:HAS_TAG]->(t)",
            "MATCH (f:Forum)-[:CONTAINER_OF]->(p:Post {id: 63})-[r]->(x)
             RETURN f.id AS forum, p.content AS content, p.imageFile AS image, type(r) AS t, x.id AS id
             ORDER BY t, id"
                .into(),
            vec![
                vec!["50", "'hola'", "null", "'HAS_CREATOR'", "4"],
                vec!["50", "'hola'", "null", "'HAS_TAG'", "30"],
                vec!["50", "'hola'", "null", "'IS_LOCATED_IN'", "11"],
            ],
        ),
        (
            "IU7",
            "MATCH (author:Person {id: 1}), (country:Country {id: 10}), (message:Message {id: 62})
             CREATE (author)<-[:HAS_CREATOR]-(c:Comment:Message {id: 64, creationDate: 2500,
               locationIP: '1.1.1.1', browserUsed: 'Firefox', content: 'great', length: 5})
               -[:REPLY_OF]->(message),
               (c)-[:IS_LOCATED_IN]->(country)
             WITH c
             UNWIND [31] AS tagId
             MATCH (t:Tag {id: tagId})
             CREATE (c)-[:HAS_TAG]->(t)",
            format!("MATCH (:Comment {{id: 64}})-[r]->(x) {edges_from}"),
            vec![
                vec!["'HAS_CREATOR'", "1"],
                vec!["'HAS_TAG'", "31"],
                vec!["'IS_LOCATED_IN'", "10"],
                vec!["'REPLY_OF'", "62"],
            ],
        ),
        (
            "IU8",
            "MATCH (p1:Person {id: 1}), (p2:Person {id: 4})
             CREATE (p1)-[:KNOWS {creationDate: 2600}]->(p2)",
            "MATCH (:Person {id: 1})-[k:KNOWS]->(p) RETURN p.id AS id, k.creationDate AS at ORDER BY id"
                .into(),
            vec![vec!["2", "5"], vec!["4", "2600"]],
        ),
    ]
}
