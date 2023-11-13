// Copyright (C) 2023 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use crate::not_nan_f32::NotNaNf32;
use crate::query_ast::field_presence::FieldPresenceQuery;
use crate::query_ast::user_input_query::UserInputQuery;
use crate::query_ast::{
    BoolQuery, FullTextQuery, PhrasePrefixQuery, QueryAst, RangeQuery, TermQuery, TermSetQuery,
    WildcardQuery,
};

/// Simple trait to implement a Visitor over the QueryAst.
pub trait QueryAstVisitor<'a> {
    type Err;

    fn visit(&mut self, query_ast: &'a QueryAst) -> Result<(), Self::Err> {
        match query_ast {
            QueryAst::Bool(bool_query) => self.visit_bool(bool_query),
            QueryAst::Term(term_query) => self.visit_term(term_query),
            QueryAst::TermSet(term_set_query) => self.visit_term_set(term_set_query),
            QueryAst::FullText(full_text_query) => self.visit_full_text(full_text_query),
            QueryAst::PhrasePrefix(phrase_prefix_query) => {
                self.visit_phrase_prefix(phrase_prefix_query)
            }
            QueryAst::Range(range_query) => self.visit_range(range_query),
            QueryAst::MatchAll => self.visit_match_all(),
            QueryAst::MatchNone => self.visit_match_none(),
            QueryAst::Boost { underlying, boost } => self.visit_boost(underlying, *boost),
            QueryAst::UserInput(user_text_query) => self.visit_user_text(user_text_query),
            QueryAst::FieldPresence(exists) => self.visit_exists(exists),
            QueryAst::Wildcard(wildcard) => self.visit_wildcard(wildcard),
        }
    }

    fn visit_bool(&mut self, bool_query: &'a BoolQuery) -> Result<(), Self::Err> {
        for ast in bool_query
            .must
            .iter()
            .chain(bool_query.should.iter())
            .chain(bool_query.must_not.iter())
            .chain(bool_query.filter.iter())
        {
            self.visit(ast)?;
        }
        Ok(())
    }

    fn visit_term(&mut self, _term_query: &'a TermQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_term_set(&mut self, _term_query: &'a TermSetQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_full_text(&mut self, _full_text: &'a FullTextQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_phrase_prefix(
        &mut self,
        _phrase_query: &'a PhrasePrefixQuery,
    ) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_match_all(&mut self) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_match_none(&mut self) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_boost(
        &mut self,
        underlying: &'a QueryAst,
        _boost: NotNaNf32,
    ) -> Result<(), Self::Err> {
        self.visit(underlying)
    }

    fn visit_range(&mut self, _range_query: &'a RangeQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_user_text(&mut self, _user_text_query: &'a UserInputQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_exists(&mut self, _exists_query: &'a FieldPresenceQuery) -> Result<(), Self::Err> {
        Ok(())
    }

    fn visit_wildcard(&mut self, _wildcard_query: &'a WildcardQuery) -> Result<(), Self::Err> {
        Ok(())
    }
}
