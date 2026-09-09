use polars::prelude::*;

fn to_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 8);
    let mut prev_is_lower_or_digit = false;

    for c in s.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() {
                // Insert underscore before an uppercase letter that follows
                // a lowercase letter or digit (handles camelCase -> camel_case)
                if prev_is_lower_or_digit {
                    result.push('_');
                }
                result.extend(c.to_lowercase());
                prev_is_lower_or_digit = false;
            } else {
                result.push(c);
                prev_is_lower_or_digit = true;
            }
        } else {
            // Any non-alphanumeric char (space, -, ., /, etc.) becomes an underscore
            if !result.ends_with('_') && !result.is_empty() {
                result.push('_');
            }
            prev_is_lower_or_digit = false;
        }
    }

    // Trim leading/trailing underscores and collapse doubles
    let trimmed = result.trim_matches('_');
    let mut cleaned = String::with_capacity(trimmed.len());
    let mut last_was_underscore = false;
    for c in trimmed.chars() {
        if c == '_' {
            if !last_was_underscore {
                cleaned.push(c);
            }
            last_was_underscore = true;
        } else {
            cleaned.push(c);
            last_was_underscore = false;
        }
    }
    cleaned
}

pub fn rename_columns_snake_case(lf: LazyFrame) -> PolarsResult<LazyFrame> {
    let schema = lf.clone().collect_schema()?;
    let old_names: Vec<String> = schema
        .iter_names()
        .map(|n| n.to_string())
        .collect();
    let new_names: Vec<String> = old_names.iter().map(|n| to_snake_case(n)).collect();

    Ok(lf.rename(&old_names, &new_names, true))
}


#[cfg(test)]
mod tests {
    use super::*;

    // ---- to_snake_case ----

    #[test]
    fn test_simple_lowercase() {
        assert_eq!(to_snake_case("price"), "price");
    }

    #[test]
    fn test_camel_case() {
        assert_eq!(to_snake_case("openPrice"), "open_price");
        assert_eq!(to_snake_case("HighPriceUSD"), "high_price_usd");
    }

    #[test]
    fn test_spaces_and_dashes() {
        assert_eq!(to_snake_case("Open Price"), "open_price");
        assert_eq!(to_snake_case("close-price"), "close_price");
    }

    #[test]
    fn test_dots_and_slashes() {
        assert_eq!(to_snake_case("price.usd"), "price_usd");
        assert_eq!(to_snake_case("bid/ask"), "bid_ask");
    }

    #[test]
    fn test_special_characters_stripped() {
        assert_eq!(to_snake_case("P&L (%)"), "p_l");
        assert_eq!(to_snake_case("Volume@Close!"), "volume_close");
    }

    #[test]
    fn test_repeated_separators_collapse() {
        assert_eq!(to_snake_case("Open   Price---USD"), "open_price_usd");
        assert_eq!(to_snake_case("__Weird__Name__"), "weird_name");
    }

    #[test]
    fn test_leading_trailing_separators_trimmed() {
        assert_eq!(to_snake_case("  Ticker "), "ticker");
        assert_eq!(to_snake_case("-Value-"), "value");
    }

    #[test]
    fn test_all_caps_acronym() {
        // Consecutive uppercase letters don't each get their own underscore
        assert_eq!(to_snake_case("USDJPY"), "usdjpy");
        assert_eq!(to_snake_case("NAV_USD"), "nav_usd");
    }

    #[test]
    fn test_digits_preserved() {
        assert_eq!(to_snake_case("Return2024"), "return2024");
        assert_eq!(to_snake_case("Q4Revenue"), "q4_revenue");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(to_snake_case(""), "");
    }

    #[test]
    fn test_only_special_characters() {
        assert_eq!(to_snake_case("!!!"), "");
    }

    // ---- rename_columns_snake_case ----

    #[test]
    fn test_rename_columns_on_lazyframe() -> PolarsResult<()> {
        let df = df![
            "Open Price" => [1.0, 2.0],
            "closePrice" => [3.0, 4.0],
            "Volume(USD)" => [100, 200],
        ]?;

        let renamed = rename_columns_snake_case(df.lazy())?.collect()?;
        let names: Vec<String> = renamed
            .get_column_names()
            .iter()
            .map(|n| n.to_string())
            .collect();

        assert_eq!(
            names,
            vec!["open_price", "close_price", "volume_usd"]
        );
        Ok(())
    }

    #[test]
    fn test_rename_preserves_data() -> PolarsResult<()> {
        let df = df![
            "My Column" => [1, 2, 3],
        ]?;

        let renamed = rename_columns_snake_case(df.lazy())?.collect()?;
        let col = renamed.column("my_column")?;
        assert_eq!(col.i32()?.into_no_null_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
        Ok(())
    }

    #[test]
    fn test_rename_already_snake_case_is_idempotent() -> PolarsResult<()> {
        let df = df![
            "already_snake" => [1],
            "also_fine" => [2],
        ]?;

        let renamed = rename_columns_snake_case(df.lazy())?.collect()?;
        let names: Vec<String> = renamed
            .get_column_names()
            .iter()
            .map(|n| n.to_string())
            .collect();

        assert_eq!(names, vec!["already_snake", "also_fine"]);
        Ok(())
    }
}