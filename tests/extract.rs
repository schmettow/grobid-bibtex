//! End-to-end extraction against a mock GROBID server.
//!
//! The mock implements `/api/isalive` and `/api/processHeaderDocument` with a
//! fixed TEI fixture, so the whole chain — HTTP client, multipart upload,
//! TEI parser and the bounded worker pool — runs without a real server.

use std::net::SocketAddr;

use grobid_bibtex::extract;
use grobid_bibtex::{GrobidClient, ProcessOptions};

/// The title "Tiny." stays below the OpenAlex search threshold, so nothing in
/// these tests reaches the OpenAlex API. Its trailing period exercises the
/// parser's title cleanup.
const MOCK_TEI: &str = r#"<TEI xmlns="http://www.tei-c.org/ns/1.0">
    <teiHeader>
        <encodingDesc><appInfo>
            <application version="0.9.1" when="2026-01-01T00:00+0000"/>
        </appInfo></encodingDesc>
        <fileDesc>
            <titleStmt><title level="a" type="main">Tiny</title></titleStmt>
            <publicationStmt><publisher/></publicationStmt>
            <sourceDesc>
                <biblStruct>
                    <analytic>
                        <title level="a" type="main">Tiny.</title>
                        <author><persName>
                            <forename type="first">Jane</forename>
                            <surname>Smith</surname>
                        </persName></author>
                        <author><persName>
                            <forename type="first">Ann</forename>
                            <surname>Jones</surname>
                        </persName></author>
                    </analytic>
                    <monogr><imprint><date type="published" when="2020"/></imprint></monogr>
                </biblStruct>
            </sourceDesc>
        </fileDesc>
    </teiHeader>
    <text/>
</TEI>"#;

/// A minimal mock of the two GROBID endpoints these tests use.
async fn spawn_mock_grobid(tei: &'static str) -> SocketAddr {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Some(head) = read_request_head(&mut stream).await else {
                    return;
                };
                let request_line = head.lines().next().unwrap_or("");
                let (status, body) = if request_line.contains("/api/isalive") {
                    ("200 OK", "true".to_string())
                } else if request_line.contains("/api/processHeaderDocument") {
                    ("200 OK", tei.to_string())
                } else {
                    ("404 Not Found", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

/// Read a full request (headers and body, by `Content-Length`) and return the
/// header block. Keeping the whole body in the read path avoids closing the
/// connection while the client is still sending.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        if buf.len() >= header_end + content_length {
            return Some(head);
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::test]
async fn test_headers_end_to_end_against_mock_server() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first.pdf");
    let second = dir.path().join("second.pdf");
    for path in [&first, &second] {
        std::fs::write(path, b"%PDF-1.4 fake").expect("write pdf");
    }

    let addr = spawn_mock_grobid(MOCK_TEI).await;
    let client = GrobidClient::new(format!("http://{addr}")).expect("client");
    let pdfs = vec![first.clone(), second.clone()];
    let outcomes = extract::headers(&client, pdfs, ProcessOptions::default(), 2).await;

    // One result per PDF, in input order, with the parsed header metadata.
    assert_eq!(outcomes.len(), 2);
    let first_extracted = outcomes[0].as_ref().expect("first pdf processed");
    assert_eq!(first_extracted.path, first);
    assert_eq!(first_extracted.record.title.as_deref(), Some("Tiny"));
    assert_eq!(first_extracted.record.authors.len(), 2);
    assert_eq!(first_extracted.record.date.as_deref(), Some("2020"));
    assert_eq!(
        outcomes[1].as_ref().expect("second pdf processed").path,
        second
    );
}

#[cfg(feature = "openalex")]
#[tokio::test]
async fn test_complete_biblios_without_usable_title_needs_no_network() {
    use grobid_bibtex::openalex::Completer;
    use grobid_bibtex::{Biblio, complete};

    let records = vec![(
        std::path::PathBuf::from("tiny.pdf"),
        Biblio {
            title: Some("Tiny.".to_string()),
            ..Biblio::default()
        },
    )];
    let report = complete::biblios(&Completer::new(), records, 1).await;

    assert_eq!(report.matched, 0);
    assert!(report.failures.is_empty());
    assert_eq!(report.records.len(), 1);
    assert_eq!(report.records[0].1.title.as_deref(), Some("Tiny."));
}
