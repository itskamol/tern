//! What VoiceOver reads for the sidebar's rows and group headers. AccessKit's macOS adapter
//! does not pass `expanded` on, so a group's state is spoken in its label.

use tern_ssh::HostEntry;

use crate::indicator::Dot;

/// A host row: the name, where it connects, and how its newest tab is doing.
pub(super) fn host_label(host: &HostEntry, status: Option<Dot>) -> String {
    let mut label = host.alias.clone();
    let address = super::address(host);
    if address != host.alias {
        label.push_str(", ");
        label.push_str(&address);
    }
    if let Some(words) = status.and_then(crate::tabs::status_words) {
        label.push_str(", ");
        label.push_str(words);
    }
    label
}

/// A group header: its name, how many hosts it holds, and whether they are showing.
pub(super) fn group_label(name: &str, hosts: usize, collapsed: bool) -> String {
    let count = match hosts {
        1 => "1 host".to_owned(),
        n => format!("{n} hosts"),
    };
    let state = if collapsed { "collapsed" } else { "expanded" };
    format!("{name}, {count}, {state}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str, host_name: &str, user: Option<&str>) -> HostEntry {
        HostEntry {
            alias: alias.into(),
            host_name: host_name.into(),
            port: 22,
            user: user.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn a_host_is_read_as_name_then_address_then_status() {
        let h = host("web", "10.0.0.5", Some("deploy"));
        assert_eq!(host_label(&h, None), "web, deploy@10.0.0.5");
        assert_eq!(
            host_label(&h, Some(Dot::Connected)),
            "web, deploy@10.0.0.5, Connected"
        );
    }

    #[test]
    fn an_address_that_repeats_the_name_is_not_read_twice() {
        assert_eq!(host_label(&host("web", "web", None), None), "web");
    }

    #[test]
    fn a_host_that_is_not_connected_says_nothing_about_it() {
        let h = host("web", "web", None);
        assert_eq!(host_label(&h, Some(Dot::Idle)), "web");
        assert_eq!(
            host_label(&h, Some(Dot::WaitingForInput)),
            "web, Waiting for your answer"
        );
    }

    #[test]
    fn a_group_says_how_many_hosts_and_whether_they_show() {
        assert_eq!(group_label("prod", 3, true), "prod, 3 hosts, collapsed");
        assert_eq!(group_label("prod", 1, false), "prod, 1 host, expanded");
    }
}
