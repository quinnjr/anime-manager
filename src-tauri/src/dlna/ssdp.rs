use super::ids::xml_escape;
use super::server::DlnaServer;

/// Minimal SCPD for the advertised ContentDirectory service: the one action
/// renderers actually call. Served so the SCPDURLs in device_xml are not 404s.
pub(crate) const SCPD_CONTENT_DIRECTORY: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList><action><name>Browse</name>
<argumentList>
<argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument>
<argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument>
<argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument>
<argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument>
<argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument>
<argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument>
<argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument>
<argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument>
</argumentList></action></actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable>
</serviceStateTable>
</scpd>"#;

/// Minimal SCPD for ConnectionManager: GetProtocolInfo only, which is all we
/// implement on that service.
pub(crate) const SCPD_CONNECTION_MANAGER: &str = r#"<?xml version="1.0"?>
<scpd xmlns="urn:schemas-upnp-org:service-1-0">
<specVersion><major>1</major><minor>0</minor></specVersion>
<actionList><action><name>GetProtocolInfo</name>
<argumentList>
<argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument>
<argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument>
</argumentList></action></actionList>
<serviceStateTable>
<stateVariable sendEvents="no"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable>
<stateVariable sendEvents="no"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable>
</serviceStateTable>
</scpd>"#;

const SSDP_MULTICAST: &str = "239.255.255.250:1900";
const SSDP_MAX_AGE: u32 = 1800;
const MEDIA_SERVER_ST: &str = "urn:schemas-upnp-org:device:MediaServer:1";
pub(crate) const DLNA_CONTENT_FEATURES: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

fn server_id() -> String {
    format!("anime-manager/{} UPnP/1.0", env!("CARGO_PKG_VERSION"))
}

/// Best-effort LAN address for the SSDP LOCATION line. Connecting a UDP
/// socket sends no packets; it just picks the interface that would route.
/// Falls back to loopback when there is no route — loudly, since renderers
/// on the LAN cannot reach a loopback LOCATION.
pub(crate) fn local_ip() -> std::net::IpAddr {
    match std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect("239.255.255.250:1900")?;
        s.local_addr()
    }) {
        Ok(a) => a.ip(),
        Err(e) => {
            eprintln!("dlna: cannot determine LAN address ({e}); advertising loopback");
            std::net::IpAddr::from([127, 0, 0, 1])
        }
    }
}

// ---------------------------------------------------------------------------
// SSDP discovery
// ---------------------------------------------------------------------------

/// Reply to one M-SEARCH packet, or `None` when it is not a search for this
/// device. The ST value is echoed, per UPnP §1.3.2.
pub fn ssdp_msearch_reply(
    request: &str,
    ip: &std::net::IpAddr,
    port: u16,
    uuid: &str,
) -> Option<String> {
    let mut lines = request.lines();
    if !lines.next().unwrap_or("").starts_with("M-SEARCH") {
        return None;
    }
    let mut st: Option<String> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("st")
        {
            st = Some(v.trim().trim_matches('"').to_string());
        }
    }
    let st = st?;
    if st != MEDIA_SERVER_ST && st != "ssdp:all" && st != "upnp:rootdevice" {
        return None;
    }
    Some(format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={age}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         SERVER: {server}\r\n\
         ST: {st}\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        age = SSDP_MAX_AGE,
        server = server_id(),
        dev = MEDIA_SERVER_ST,
    ))
}

pub(crate) fn notify_alive(ip: &std::net::IpAddr, port: u16, uuid: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: {mc}\r\n\
         CACHE-CONTROL: max-age={age}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         NT: {dev}\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: {server}\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        mc = SSDP_MULTICAST,
        age = SSDP_MAX_AGE,
        server = server_id(),
        dev = MEDIA_SERVER_ST,
    )
}

pub(crate) fn notify_byebye(ip: &std::net::IpAddr, port: u16, uuid: &str) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: {mc}\r\n\
         LOCATION: http://{ip}:{port}/desc.xml\r\n\
         NT: {dev}\r\n\
         NTS: ssdp:byebye\r\n\
         USN: {uuid}::{dev}\r\n\
         \r\n",
        mc = SSDP_MULTICAST,
        dev = MEDIA_SERVER_ST,
    )
}

pub(crate) async fn send_notify(msg: &str) -> std::io::Result<()> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(msg.as_bytes(), SSDP_MULTICAST).await?;
    Ok(())
}

pub(crate) async fn ssdp_socket() -> std::io::Result<tokio::net::UdpSocket> {
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:1900").await?;
    sock.join_multicast_v4(
        "239.255.255.250".parse().unwrap(),
        std::net::Ipv4Addr::UNSPECIFIED,
    )?;
    Ok(sock)
}

pub(crate) async fn ssdp_responder(
    sock: tokio::net::UdpSocket,
    ip: std::net::IpAddr,
    port: u16,
    uuid: String,
    clients_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            r = sock.recv_from(&mut buf) => {
                let Ok((n, peer)) = r else { continue };
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(reply) = ssdp_msearch_reply(&req, &ip, port, &uuid)
                    && sock.send_to(reply.as_bytes(), peer).await.is_ok()
                {
                    clients_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

impl DlnaServer {
    pub fn device_xml(&self) -> String {
        format!(
            "<?xml version=\"1.0\"?>\r\n\
             <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\
             <specVersion><major>1</major><minor>0</minor></specVersion>\
             <device>\
             <deviceType>{st}</deviceType>\
             <friendlyName>{name}</friendlyName>\
             <manufacturer>anime-manager</manufacturer>\
             <modelName>anime-manager</modelName>\
             <UDN>{udn}</UDN>\
             <serviceList>\
             <service>\
             <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>\
             <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>\
              <SCPDURL>/scpd/ContentDirectory.xml</SCPDURL>\
              <controlURL>/ctl/ContentDirectory</controlURL>\
              </service>\
              <service>\
              <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>\
              <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>\
              <SCPDURL>/scpd/ConnectionManager.xml</SCPDURL>\
              <controlURL>/ctl/ConnectionManager</controlURL>\
             </service>\
             </serviceList>\
             </device>\
             </root>",
            st = MEDIA_SERVER_ST,
            name = xml_escape(&self.name),
            udn = xml_escape(&self.uuid),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ssdp_msearch_reply_matches_spec() {
        let req = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                   ST: urn:schemas-upnp-org:device:MediaServer:1\r\nMAN: \"ns=01\"\r\nMX: 3\r\n\r\n";
        let ip: std::net::IpAddr = "192.168.1.5".parse().unwrap();
        let server = format!("anime-manager/{} UPnP/1.0", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            crate::dlna::ssdp_msearch_reply(req, &ip, 8200, "uuid:test").unwrap(),
            format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\n\
                 LOCATION: http://192.168.1.5:8200/desc.xml\r\nSERVER: {server}\r\n\
                 ST: urn:schemas-upnp-org:device:MediaServer:1\r\n\
                 USN: uuid:test::urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n"
            )
        );
        // ssdp:all is answered, other device types and NOTIFY packets are not.
        let all = req.replace("urn:schemas-upnp-org:device:MediaServer:1", "ssdp:all");
        assert!(crate::dlna::ssdp_msearch_reply(&all, &ip, 8200, "uuid:test").is_some());
        let other = req.replace("MediaServer:1", "Printer:1");
        assert!(crate::dlna::ssdp_msearch_reply(&other, &ip, 8200, "uuid:test").is_none());
        assert!(
            crate::dlna::ssdp_msearch_reply(
                "NOTIFY * HTTP/1.1\r\nNTS: ssdp:alive\r\n\r\n",
                &ip,
                8200,
                "uuid:test"
            )
            .is_none()
        );
    }

    #[test]
    fn device_xml_names_server_and_services() {
        let srv = crate::dlna::DlnaServer {
            port: 8200,
            name: "Shelf & Spine <test>".into(),
            uuid: "uuid:abc".into(),
            clients_seen: Default::default(),
        };
        let xml = srv.device_xml();
        assert!(xml.contains("<friendlyName>Shelf &amp; Spine &lt;test&gt;</friendlyName>"));
        assert!(xml.contains("<UDN>uuid:abc</UDN>"));
        assert!(xml.contains("urn:schemas-upnp-org:device:MediaServer:1"));
        assert!(xml.contains("<controlURL>/ctl/ContentDirectory</controlURL>"));
        assert!(xml.contains("<controlURL>/ctl/ConnectionManager</controlURL>"));
    }

    #[test]
    fn ssdp_notify_and_search_counter_paths() {
        let ip: std::net::IpAddr = "192.168.1.5".parse().unwrap();
        // Alive and byebye NOTIFYs carry the LOCATION, the MediaServer USN,
        // and their respective NTS values.
        let alive = crate::dlna::notify_alive(&ip, 8200, "uuid:t");
        assert!(
            alive.contains("LOCATION: http://192.168.1.5:8200/desc.xml"),
            "{alive}"
        );
        assert!(
            alive.contains("USN: uuid:t::urn:schemas-upnp-org:device:MediaServer:1"),
            "{alive}"
        );
        assert!(alive.contains("NTS: ssdp:alive"), "{alive}");
        let byebye = crate::dlna::notify_byebye(&ip, 8200, "uuid:t");
        assert!(
            byebye.contains("LOCATION: http://192.168.1.5:8200/desc.xml"),
            "{byebye}"
        );
        assert!(
            byebye.contains("USN: uuid:t::urn:schemas-upnp-org:device:MediaServer:1"),
            "{byebye}"
        );
        assert!(byebye.contains("NTS: ssdp:byebye"), "{byebye}");
        // A loopback M-SEARCH is answered with a loopback LOCATION (no real
        // multicast needed), and answering bumps the counter exactly once —
        // the same accounting `ssdp_responder` performs per reply.
        let req = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                   ST: urn:schemas-upnp-org:device:MediaServer:1\r\nMAN: \"ns=01\"\r\nMX: 3\r\n\r\n";
        let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let seen: std::sync::Arc<std::sync::atomic::AtomicU64> = Default::default();
        let reply = crate::dlna::ssdp_msearch_reply(req, &loopback, 8200, "uuid:t");
        let reply = reply.expect("loopback search is answered");
        assert!(
            reply.contains("LOCATION: http://127.0.0.1:8200/desc.xml"),
            "{reply}"
        );
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
