use anyhow::Result;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query, QueryParser};
use tantivy::query_grammar::{Delimiter, UserInputAst, UserInputLeaf};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Index, Score, Term};

use crate::schema::IndexFields;
use crate::search::{SearchIndex, SearchScope};
use crate::tokenize::{compact_identifier, dedup_preserving_order, tokenized_query_terms};

pub fn query_uses_structured_syntax(query: &str) -> bool {
    tantivy::query_grammar::parse_query(query.trim()).is_ok_and(|ast| ast_uses_query_syntax(&ast))
}

impl SearchIndex {
    pub(crate) fn build_query(
        &self,
        query_text: &str,
        scopes: &[SearchScope],
    ) -> Result<Box<dyn Query>> {
        let text_query = self.build_text_query(query_text)?;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];

        if !scopes.is_empty() {
            let mut scope_clauses = Vec::with_capacity(scopes.len());

            for scope in scopes {
                let document_kind = scope.entry_kind.document_kind();
                let source_query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
                    Term::from_field_text(self.fields.source, &scope.source),
                    tantivy::schema::IndexRecordOption::Basic,
                ));

                let ref_query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
                    Term::from_field_text(self.fields.ref_id, &scope.ref_id),
                    tantivy::schema::IndexRecordOption::Basic,
                ));

                let kind_query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
                    Term::from_field_text(self.fields.kind, document_kind.as_str()),
                    tantivy::schema::IndexRecordOption::Basic,
                ));

                let pair_query: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                    (Occur::Must, source_query),
                    (Occur::Must, ref_query),
                    (Occur::Must, kind_query),
                ]));

                scope_clauses.push((Occur::Should, pair_query));
            }

            clauses.push((Occur::Must, Box::new(BooleanQuery::new(scope_clauses))));
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn build_text_query(&self, query_text: &str) -> Result<Box<dyn Query>> {
        let Ok(user_input_ast) = tantivy::query_grammar::parse_query(query_text.trim()) else {
            return self.build_boosted_text_query(query_text);
        };

        if ast_uses_query_syntax(&user_input_ast) {
            return self.build_parsed_text_query(query_text, user_input_ast);
        }

        self.build_boosted_text_query(query_text)
    }

    fn build_parsed_text_query(
        &self,
        query_text: &str,
        user_input_ast: UserInputAst,
    ) -> Result<Box<dyn Query>> {
        let scoring_query = self.build_boosted_text_query(query_text)?;
        let Ok(parsed_query) = self
            .query_parser(&user_input_ast)
            .build_query_from_user_input_ast(user_input_ast)
        else {
            return Ok(scoring_query);
        };

        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, parsed_query),
            (Occur::Should, scoring_query),
        ])))
    }

    fn build_boosted_text_query(&self, query_text: &str) -> Result<Box<dyn Query>> {
        let mut text_clauses = self.build_exact_text_clauses(query_text)?;

        if let Some(fuzzy_query) = build_fuzzy_query(&self.index, query_text, &self.fields)? {
            text_clauses.push((Occur::Should, fuzzy_query));
        }

        Ok(Box::new(BooleanQuery::new(text_clauses)))
    }

    fn query_parser(&self, user_input_ast: &UserInputAst) -> QueryParser {
        let mut parser = QueryParser::for_index(
            &self.index,
            self.query_parser_default_fields(user_input_ast),
        );

        self.set_query_parser_boosts(&mut parser);

        parser
    }

    fn query_parser_default_fields(&self, user_input_ast: &UserInputAst) -> Vec<Field> {
        let mut fields = vec![
            self.fields.name_text,
            self.fields.attribute_text,
            self.fields.description,
            self.fields.option_type,
        ];

        if !ast_contains_quoted_literal(user_input_ast) {
            fields.extend([
                self.fields.name_exact,
                self.fields.name_groups,
                self.fields.name_root,
                self.fields.name_leaf,
                self.fields.attribute_exact,
                self.fields.main_program,
                self.fields.programs,
                self.fields.option_set,
                self.fields.parents,
                self.fields.package_set,
                self.fields.platforms,
            ]);
        }

        fields
    }

    fn set_query_parser_boosts(&self, parser: &mut QueryParser) {
        parser.set_field_boost(self.fields.name_text, 10.0);
        parser.set_field_boost(self.fields.attribute_text, 12.0);
        parser.set_field_boost(self.fields.description, 1.0);
        parser.set_field_boost(self.fields.option_type, 1.5);

        parser.set_field_boost(self.fields.name_exact, 20.0);
        parser.set_field_boost(self.fields.name_groups, 15.0);
        parser.set_field_boost(self.fields.name_root, 8.0);
        parser.set_field_boost(self.fields.name_leaf, 6.0);
        parser.set_field_boost(self.fields.attribute_exact, 25.0);
        parser.set_field_boost(self.fields.main_program, 20.0);
        parser.set_field_boost(self.fields.programs, 14.0);
        parser.set_field_boost(self.fields.option_set, 3.0);
        parser.set_field_boost(self.fields.parents, 2.0);
        parser.set_field_boost(self.fields.package_set, 2.0);
        parser.set_field_boost(self.fields.platforms, 1.0);
    }

    fn build_exact_text_clauses(&self, query_text: &str) -> Result<Vec<(Occur, Box<dyn Query>)>> {
        let mut clauses = Vec::new();
        let raw = query_text.trim();

        if !raw.is_empty() {
            for value in dedup_preserving_order(vec![raw.to_owned(), raw.to_lowercase()]) {
                self.add_string_field_clauses(&mut clauses, &value);
            }
        }

        let mut terms = tokenized_query_terms(&self.index, self.fields.name_text, query_text)?;
        let compact = compact_identifier(query_text);

        if !compact.is_empty() {
            terms.push(compact);
        }

        for term in dedup_preserving_order(terms) {
            self.add_token_field_clauses(&mut clauses, &term);
        }

        Ok(clauses)
    }

    fn add_string_field_clauses(&self, clauses: &mut Vec<(Occur, Box<dyn Query>)>, value: &str) {
        add_boosted_term_clause(
            clauses,
            self.fields.name_exact,
            value,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_groups,
            value,
            15.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_root,
            value,
            8.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_leaf,
            value,
            6.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.attribute_exact,
            value,
            25.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.main_program,
            value,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.programs,
            value,
            14.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_set,
            value,
            3.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.parents,
            value,
            2.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.package_set,
            value,
            2.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.platforms,
            value,
            1.0,
            IndexRecordOption::Basic,
        );
    }

    fn add_token_field_clauses(&self, clauses: &mut Vec<(Occur, Box<dyn Query>)>, term: &str) {
        add_boosted_term_clause(
            clauses,
            self.fields.name_text,
            term,
            10.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.attribute_text,
            term,
            12.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.description,
            term,
            1.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_type,
            term,
            1.5,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_root,
            term,
            8.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_leaf,
            term,
            6.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.main_program,
            term,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.programs,
            term,
            14.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_set,
            term,
            3.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.package_set,
            term,
            2.0,
            IndexRecordOption::Basic,
        );
    }
}

fn ast_uses_query_syntax(ast: &UserInputAst) -> bool {
    match ast {
        UserInputAst::Clause(clauses) => clauses
            .iter()
            .any(|(occur, child)| occur.is_some() || ast_uses_query_syntax(child)),
        UserInputAst::Boost(_, _) => true,
        UserInputAst::Leaf(leaf) => leaf_uses_query_syntax(leaf),
    }
}

fn leaf_uses_query_syntax(leaf: &UserInputLeaf) -> bool {
    match leaf {
        UserInputLeaf::Literal(literal) => {
            literal.field_name.is_some()
                || literal.delimiter != Delimiter::None
                || literal.slop != 0
                || literal.prefix
        }
        UserInputLeaf::All
        | UserInputLeaf::Range { .. }
        | UserInputLeaf::Set { .. }
        | UserInputLeaf::Exists { .. }
        | UserInputLeaf::Regex { .. } => true,
    }
}

fn ast_contains_quoted_literal(ast: &UserInputAst) -> bool {
    match ast {
        UserInputAst::Clause(clauses) => clauses
            .iter()
            .any(|(_, child)| ast_contains_quoted_literal(child)),
        UserInputAst::Boost(child, _) => ast_contains_quoted_literal(child),
        UserInputAst::Leaf(leaf) => match leaf.as_ref() {
            UserInputLeaf::Literal(literal) => literal.delimiter != Delimiter::None,
            UserInputLeaf::All
            | UserInputLeaf::Range { .. }
            | UserInputLeaf::Set { .. }
            | UserInputLeaf::Exists { .. }
            | UserInputLeaf::Regex { .. } => false,
        },
    }
}

fn add_boosted_term_clause(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    field: Field,
    value: &str,
    boost: Score,
    index_record_option: IndexRecordOption,
) {
    if value.is_empty() {
        return;
    }

    let query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
        Term::from_field_text(field, value),
        index_record_option,
    ));

    clauses.push((Occur::Should, Box::new(BoostQuery::new(query, boost))));
}

fn build_fuzzy_query(
    index: &Index,
    query: &str,
    fields: &IndexFields,
) -> Result<Option<Box<dyn Query>>> {
    let terms = fuzzy_query_terms(index, fields.name_text, query)?;

    let mut term_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for term in terms {
        let Some(distance) = fuzzy_distance(&term) else {
            continue;
        };

        let mut field_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        add_fuzzy_field_clause(&mut field_clauses, fields.name_text, &term, distance, 10.0);
        add_fuzzy_field_clause(
            &mut field_clauses,
            fields.attribute_text,
            &term,
            distance,
            12.0,
        );

        term_clauses.push((Occur::Should, Box::new(BooleanQuery::new(field_clauses))));
    }

    if term_clauses.is_empty() {
        return Ok(None);
    }

    Ok(Some(Box::new(BooleanQuery::new(term_clauses))))
}

fn fuzzy_query_terms(index: &Index, field: Field, query: &str) -> Result<Vec<String>> {
    let mut terms = tokenized_query_terms(index, field, query)?;
    let compact = compact_identifier(query);

    if !compact.is_empty() {
        terms.push(compact);
    }

    Ok(dedup_preserving_order(terms))
}

fn add_fuzzy_field_clause(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    field: Field,
    term: &str,
    distance: u8,
    field_boost: f32,
) {
    let fuzzy_query: Box<dyn Query> = Box::new(FuzzyTermQuery::new_prefix(
        Term::from_field_text(field, term),
        distance,
        true,
    ));

    clauses.push((
        Occur::Should,
        Box::new(BoostQuery::new(
            fuzzy_query,
            fuzzy_boost(distance, field_boost),
        )),
    ));
}

fn fuzzy_distance(term: &str) -> Option<u8> {
    match term.chars().count() {
        0..=2 => None,
        3..=7 => Some(1),
        _ => Some(2),
    }
}

fn fuzzy_boost(distance: u8, field_boost: f32) -> f32 {
    let distance_boost = match distance {
        0 => 1.0,
        1 => 0.25,
        _ => 0.10,
    };

    field_boost * distance_boost
}

#[cfg(test)]
mod tests {
    use super::query_uses_structured_syntax;

    #[test]
    fn detects_boolean_word_operators_as_structured() {
        assert!(query_uses_structured_syntax("firefox AND programs"));
        assert!(query_uses_structured_syntax("firefox OR chromium"));
        assert!(query_uses_structured_syntax("firefox NOT programs"));
    }

    #[test]
    fn detects_symbol_and_field_syntax_as_structured() {
        assert!(query_uses_structured_syntax("firefox -programs"));
        assert!(query_uses_structured_syntax("name:Git"));
        assert!(query_uses_structured_syntax("\"Git Grep\""));
    }

    #[test]
    fn plain_text_queries_are_not_structured() {
        assert!(!query_uses_structured_syntax("Git Grep"));
        assert!(!query_uses_structured_syntax("git   grep"));
    }

    #[test]
    fn malformed_queries_are_not_structured() {
        assert!(!query_uses_structured_syntax("\"unclosed"));
    }
}
