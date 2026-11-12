//! An endpoint's request as a method and a URI template whose variables map to
//! the typed params of the endpoint's function.
//!
//! Only simple expansion (`{name}`) is supported, with an optional type
//! (`{name:u32}`) that defaults to `string`.

use anyhow::{Result, bail};

/// The HTTP methods the generated component can send. Request bodies are not
/// yet supported, so only methods without one are included.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    Get,
    Head,
    Delete,
    Options,
}

impl Method {
    /// The method's case in the client's `method` enum.
    pub fn case(self) -> &'static str {
        match self {
            Method::Get => "get",
            Method::Head => "head",
            Method::Delete => "delete",
            Method::Options => "options",
        }
    }

    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "GET" => Method::Get,
            "HEAD" => Method::Head,
            "DELETE" => Method::Delete,
            "OPTIONS" => Method::Options,
            "POST" | "PUT" | "PATCH" | "QUERY" => {
                bail!("{name} requires a request body, which is not yet supported")
            }
            other => bail!("unknown HTTP method '{other}'"),
        })
    }
}

/// The WIT types a template variable may declare.
const VARIABLE_TYPES: &[&str] = &[
    "string", "bool", "s8", "s16", "s32", "s64", "u8", "u16", "u32", "u64", "f32", "f64",
];

/// One part of a URI template.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Literal(String),
    /// A variable by name, expanded from the param of the same name.
    Variable(String),
}

/// A variable's name and its WIT type, which become a function param.
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub ty: String,
}

/// A method and the URI template it is sent to.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub method: Method,
    pub parts: Vec<Part>,
    /// Each distinct variable, in the order it first appears.
    pub variables: Vec<Variable>,
}

impl Template {
    /// Parse `METHOD uri-template`, such as
    /// `GET https://api.weather.gov/gridpoints/{wfo}/{x:u32},{y:u32}/forecast`.
    pub fn parse(line: &str) -> Result<Self> {
        let Some((method, uri)) = line.trim().split_once(' ') else {
            bail!("expected 'METHOD uri-template', got '{line}'");
        };
        let method = Method::parse(method)?;
        let uri = uri.trim();

        let mut parts = Vec::new();
        let mut variables: Vec<Variable> = Vec::new();
        let mut rest = uri;
        while let Some(open) = rest.find('{') {
            if open > 0 {
                parts.push(Part::Literal(rest[..open].to_string()));
            }
            let Some(close) = rest[open..].find('}') else {
                bail!("unclosed '{{' in '{uri}'");
            };
            let variable = Self::variable(&rest[open + 1..open + close])?;
            match variables.iter().find(|known| known.name == variable.name) {
                Some(known) if known.ty != variable.ty => bail!(
                    "variable '{}' is declared as both {} and {}",
                    variable.name,
                    known.ty,
                    variable.ty
                ),
                Some(_) => {}
                None => variables.push(variable.clone()),
            }
            parts.push(Part::Variable(variable.name));
            rest = &rest[open + close + 1..];
        }
        if rest.contains('}') {
            bail!("unopened '}}' in '{uri}'");
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_string()));
        }
        Ok(Template {
            method,
            parts,
            variables,
        })
    }

    /// The URI template as it is expanded, with each variable's type removed:
    /// `{x:u32}` becomes `{x}`.
    pub fn uri(&self) -> String {
        self.parts
            .iter()
            .map(|part| match part {
                Part::Literal(text) => text.clone(),
                Part::Variable(name) => format!("{{{name}}}"),
            })
            .collect()
    }

    /// One `{...}` expression: a name, and a type after `:` if declared.
    fn variable(expression: &str) -> Result<Variable> {
        if let Some(operator) = expression
            .chars()
            .next()
            .filter(|c| "+#./;?&=,!@|".contains(*c))
        {
            bail!("the URI template operator '{operator}' is not supported yet");
        }
        let (name, ty) = match expression.split_once(':') {
            Some((name, ty)) => (name, ty),
            None => (expression, "string"),
        };
        if name.is_empty() {
            bail!("a variable needs a name: '{{{expression}}}'");
        }
        if !VARIABLE_TYPES.contains(&ty) {
            bail!(
                "variable '{name}' has type '{ty}', expected one of {}",
                VARIABLE_TYPES.join(", ")
            );
        }
        Ok(Variable {
            name: name.to_string(),
            ty: ty.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variable(name: &str, ty: &str) -> Variable {
        Variable {
            name: name.to_string(),
            ty: ty.to_string(),
        }
    }

    #[test]
    fn literals_and_variables_alternate_in_order() {
        let template = Template::parse(
            "GET https://api.weather.gov/gridpoints/{wfo}/{x:u32},{y:u32}/forecast",
        )
        .unwrap();
        assert_eq!(template.method, Method::Get);
        assert_eq!(
            template.parts,
            [
                Part::Literal("https://api.weather.gov/gridpoints/".into()),
                Part::Variable("wfo".into()),
                Part::Literal("/".into()),
                Part::Variable("x".into()),
                Part::Literal(",".into()),
                Part::Variable("y".into()),
                Part::Literal("/forecast".into()),
            ]
        );
        assert_eq!(
            template.variables,
            [
                variable("wfo", "string"),
                variable("x", "u32"),
                variable("y", "u32")
            ]
        );
    }

    #[test]
    fn the_uri_drops_the_variable_types() {
        let template = Template::parse(
            "GET https://api.weather.gov/gridpoints/{wfo}/{x:u32},{y:u32}/forecast",
        )
        .unwrap();
        assert_eq!(
            template.uri(),
            "https://api.weather.gov/gridpoints/{wfo}/{x},{y}/forecast"
        );
    }

    #[test]
    fn a_template_may_end_with_a_variable() {
        let template =
            Template::parse("GET https://api.weather.gov/alerts/active/area/{area}").unwrap();
        assert_eq!(template.parts.last(), Some(&Part::Variable("area".into())));
    }

    #[test]
    fn a_repeated_variable_is_one_param() {
        let template = Template::parse("GET https://example.com/{id}/copy/{id}").unwrap();
        assert_eq!(template.variables, [variable("id", "string")]);
        assert_eq!(template.parts.len(), 4);
    }

    #[test]
    fn a_repeated_variable_with_another_type_is_rejected() {
        let error = Template::parse("GET https://example.com/{id:u32}/{id}").unwrap_err();
        assert!(
            format!("{error:#}").contains("'id' is declared as both u32 and string"),
            "{error:#}"
        );
    }

    #[test]
    fn a_method_with_a_body_is_rejected_for_now() {
        let error = Template::parse("POST https://example.com/items").unwrap_err();
        assert!(
            format!("{error:#}").contains("POST requires a request body"),
            "{error:#}"
        );
    }

    #[test]
    fn an_operator_is_rejected_for_now() {
        let error = Template::parse("GET https://example.com/alerts{?area}").unwrap_err();
        assert!(
            format!("{error:#}").contains("operator '?' is not supported"),
            "{error:#}"
        );
    }

    #[test]
    fn an_unknown_type_is_rejected() {
        let error = Template::parse("GET https://example.com/{n:int}").unwrap_err();
        assert!(
            format!("{error:#}").contains("variable 'n' has type 'int'"),
            "{error:#}"
        );
    }

    #[test]
    fn unbalanced_braces_are_rejected() {
        assert!(Template::parse("GET https://example.com/{id").is_err());
        assert!(Template::parse("GET https://example.com/id}").is_err());
    }

    #[test]
    fn a_line_without_a_method_is_rejected() {
        let error = Template::parse("https://example.com/").unwrap_err();
        assert!(
            format!("{error:#}").contains("expected 'METHOD uri-template'"),
            "{error:#}"
        );
    }
}
