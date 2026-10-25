// XML as a tree of elements for manifests and layouts, which keep what they do not know.
// Files with a fixed structure, like POMs and SDK catalogs, are read with serde instead.
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesDecl, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct XmlElement {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<XmlElement>,
}

impl XmlElement {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        return self.attributes.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str());
    }

    pub fn child(&self, name: &str) -> Option<&XmlElement> {
        return self.children.iter().find(|child| child.name == name);
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElement> {
        return self.children.iter().filter(move |child| child.name == name);
    }
}

/// Returns the root element. Namespaces stay part of the names (`android:id`),
/// comments and processing instructions are left out.
pub fn parse(input: &str) -> Result<XmlElement, String> {
    let mut reader = Reader::from_str(input);
    // the bottom element stands for the document itself
    let mut stack = vec![XmlElement::default()];

    let error = |reader: &Reader<&[u8]>, error: &dyn std::fmt::Display| {
        format!("at byte {}: {error}", reader.error_position())
    };

    loop {
        let event = reader.read_event()
            .map_err(|cause| error(&reader, &cause))?;

        match event {
            Event::Start(start) => {
                stack.push(element_of_tag(&start).map_err(|cause| error(&reader, &cause))?)
            }
            Event::Empty(start) => {
                let element = element_of_tag(&start).map_err(|cause| error(&reader, &cause))?;
                open_element(&mut stack).children.push(element);
            }
            // the reader checks that end tags match their start tags
            Event::End(_) => {
                let element = stack.pop().expect("an end tag follows its start tag");
                open_element(&mut stack).children.push(element);
            }
            Event::Text(text) => open_element(&mut stack).text.push_str(&text.xml10_content()),
            Event::CData(data) => open_element(&mut stack).text.push_str(&data.xml10_content()),
            Event::GeneralRef(reference) => {
                let name = reference.xml10_content();
                let resolved = match reference.resolve_char_ref()
                    .map_err(|cause| error(&reader, &cause))?
                {
                    Some(char) => char.to_string(),
                    // entities of a DTD stay as they are
                    None => resolve_predefined_entity(&name)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("&{name};")),
                };

                open_element(&mut stack).text.push_str(&resolved);
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
    }

    if stack.len() != 1 {
        return Err(format!("<{}> is never closed", open_element(&mut stack).name));
    }

    return stack.pop()
        .unwrap().children.into_iter()
        .next()
        .ok_or_else(|| "empty document".to_string());
}

/// Writes an element as XML document, without the attributes `skip` matches
pub fn write(element: &XmlElement, skip: &dyn Fn(&str) -> bool) -> String {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 4);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("utf-8"), None)))
        .expect("writing to memory");
    write_element(&mut writer, element, skip);
    let mut out = String::from_utf8(writer.into_inner()).expect("the elements are UTF-8");
    out.push('\n');

    return out;
}

fn element_of_tag(start: &BytesStart) -> Result<XmlElement, quick_xml::Error> {
    let mut attributes = vec![];

    for attribute in start.attributes() {
        let attribute = attribute?;
        let value = attribute.normalized_value(XmlVersion::Implicit1_0)?;
        attributes.push((attribute.key.as_ref().to_string(), value.into_owned()));
    }

    return Ok(XmlElement {
        name: start.name().as_ref().to_string(),
        attributes,
        ..XmlElement::default()
    });
}

/// The innermost element not closed yet
fn open_element(stack: &mut [XmlElement]) -> &mut XmlElement {
    return stack.last_mut()
        .expect("the document element is always on the stack");
}

fn write_element(writer: &mut Writer<Vec<u8>>, element: &XmlElement, skip: &dyn Fn(&str) -> bool) {
    let mut start = BytesStart::new(element.name.as_str());

    for (key, value) in element.attributes.iter().filter(|(key, _)| !skip(key)) {
        start.push_attribute((key.as_str(), value.as_str()));
    }

    let text = element.text.trim();

    if element.children.is_empty() && text.is_empty() {
        writer.write_event(Event::Empty(start))
            .expect("writing to memory");
        return;
    }

    writer.write_event(Event::Start(start.borrow()))
        .expect("writing to memory");

    if !text.is_empty() {
        writer.write_event(Event::Text(BytesText::new(text)))
            .expect("writing to memory");
    }

    for child in &element.children {
        write_element(writer, child, skip);
    }

    writer.write_event(Event::End(start.to_end()))
        .expect("writing to memory");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_parser_reads_the_tree() {
        let layout = parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <!-- comment with <tag> -->
            <LinearLayout xmlns:android="http://schemas.android.com/apk/res/android" a="x>y">
              <TextView android:text="a &amp; b &#x41;"/>
              <include layout="@layout/toolbar">text &lt;&#65;&gt; <![CDATA[x<y]]></include>
            </LinearLayout>"#,
        )
        .unwrap();

        assert_eq!(layout.name, "LinearLayout");
        assert_eq!(layout.attribute("a"), Some("x>y"));
        assert_eq!(
            layout.child("TextView").unwrap().attribute("android:text"),
            Some("a & b A")
        );
        assert_eq!(layout.child("include").unwrap().text, "text <A> x<y");
        assert_eq!(layout.children("TextView").count(), 1);

        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<a>").is_err());
    }

    #[test]
    fn xml_keeps_attributes_and_writes_them_back() {
        let manifest = parse(
            r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android" package='a.b'>
            <application android:label="A &amp; B" tools:replace="label"/>
        </manifest>"#,
        )
        .unwrap();

        assert_eq!(manifest.attribute("package"), Some("a.b"));
        assert_eq!(
            manifest
                .child("application")
                .unwrap()
                .attribute("android:label"),
            Some("A & B")
        );

        let written = write(&manifest, &|attribute| attribute.starts_with("tools:"));
        assert!(
            written.contains(r#"<application android:label="A &amp; B"/>"#),
            "{written}"
        );
        assert_eq!(parse(&written).unwrap().attribute("package"), Some("a.b"));
    }
}
