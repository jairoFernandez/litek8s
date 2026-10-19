use super::*;
use serde_json::json;

fn run(src: &str, object: Json) -> Result<Val, String> {
    let mut env = Env::default();
    env.vars.insert("object".into(), Val::from_json(&object));
    compile(src)?.eval(&env)
}

#[test]
fn expressions() {
    let d = json!({"metadata": {"name": "web", "labels": {"app": "x"}}, "spec": {"replicas": 3, "tags": ["a", "b"]}});
    assert_eq!(run("object.spec.replicas > 1", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("object.spec.replicas % 2 == 1 && object.metadata.name == 'web'", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("'got ' + string(object.spec.replicas)", d.clone()), Ok(Val::Str("got 3".into())));
    assert!(run("'got ' + object.spec.replicas", d.clone()).unwrap_err().contains("no such overload"));
    assert_eq!(run("has(object.metadata.labels) && 'app' in object.metadata.labels", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("!has(object.spec.missing)", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("object.spec.tags.all(t, t.size() == 1)", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("object.spec.tags.exists(t, t == 'b') ? 1 : 2", d.clone()), Ok(Val::Int(1)));
    assert_eq!(run("size(object.spec.tags.map(t, t + t)) == 2", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("[1, 2, 3].filter(x, x > 1)", d.clone()), Ok(Val::List(vec![Val::Int(2), Val::Int(3)])));
    assert!(run("object.spec.missing < 10", d.clone()).is_err());
    assert_eq!(run("object.spec.missing < 10 || true", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("[0, -1].all(x, 1 / x > 0)", d.clone()), Ok(Val::Bool(false)));
    assert_eq!(run("[0, 1].exists(x, 1 / x > 0)", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("object.spec.tags.all(i, v, i < 2 && v.size() == 1)", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("object.spec.tags.transformMapEntry(i, v, {v: i})", d.clone()), run("{'a': 0, 'b': 1}", d.clone()));
    assert_eq!(run("object.?spec.?nope.orValue('x')", d.clone()), Ok(Val::Str("x".into())));
    assert_eq!(run("object.metadata.labels[?'app'].hasValue()", d.clone()), Ok(Val::Bool(true)));
    assert_eq!(run("-9223372036854775808", d.clone()), Ok(Val::Int(i64::MIN)));
    assert_eq!(run("1 == 1.0 && 2u > 1 && dyn(1) == 1u", d), Ok(Val::Bool(true)));
}

#[test]
fn libraries() {
    let n = json!(null);
    let t = |src: &str| assert_eq!(run(src, n.clone()), Ok(Val::Bool(true)), "{src}");
    t("'hello'.charAt(1) == 'e' && 'hello'.indexOf('l') == 2 && 'hello'.lastIndexOf('l') == 3");
    t("'a,b,c'.split(',') == ['a', 'b', 'c'] && 'a,b,c'.split(',', 2) == ['a', 'b,c']");
    t("'hello'.substring(1, 3) == 'el' && ' x '.trim() == 'x' && 'aXa'.replace('a', 'b') == 'bXb'");
    t("['a', 'b'].join('-') == 'a-b' && '%s=%d'.format(['x', 1]) == 'x=1'");
    t("'abc123'.matches('^[a-z]+[0-9]+$') && 'x1y22'.findAll('[0-9]+') == ['1', '22'] && 'x1'.find('[0-9]') == '1'");
    t("[1, 2, 3].isSorted() && ![2, 1].isSorted() && [1, 2, 3].sum() == 6 && [3, 1].min() == 1 && ['a', 'b'].indexOf('b') == 1");
    t("sets.contains([1, 2, 3], [1, 2]) && sets.equivalent([1, 2], [2, 1]) && sets.intersects([1], [1, 5])");
    t("isURL('https://example.com:8080/p?q=1') && url('https://example.com:8080/p').getHostname() == 'example.com'");
    t("url('https://example.com:8080/p').getPort() == '8080' && url('https://x.io/a?b=c&b=d').getQuery()['b'] == ['c', 'd']");
    t("!isURL('::') && !isURL('example.com') && isURL('/absolute/path')");
    t("quantity('1Gi').isGreaterThan(quantity('1G')) && quantity('500m').compareTo(quantity('0.5')) == 0");
    t("quantity('2').add(quantity('500m')) == quantity('2.5') && quantity('1k').asInteger() == 1000 && !isQuantity('1.2.3')");
    t("isIP('10.0.0.1') && ip('10.0.0.1').family() == 4 && ip('::1').isLoopback() && !isIP('::ffff:1.2.3.4')");
    t("cidr('10.0.0.0/8').containsIP('10.1.2.3') && !cidr('10.0.0.0/8').containsIP(ip('11.0.0.1')) && cidr('10.1.0.0/16').masked() == cidr('10.1.0.0/16')");
    t("cidr('10.0.0.0/8').containsCIDR('10.1.0.0/16') && cidr('192.168.1.5/24').masked() == cidr('192.168.1.0/24') && cidr('::/0').prefixLength() == 0");
    t("!format.dns1123Label().validate('my-name').hasValue() && format.dns1123Label().validate('My_Name').hasValue()");
    t("format.named('uuid').hasValue() && !format.named('nope').hasValue()");
    t("duration('1h30m') == duration('90m') && duration('1h').getMinutes() == 60");
    t("timestamp('2024-03-01T10:00:00Z') + duration('1h') == timestamp('2024-03-01T11:00:00Z')");
    t("timestamp('2024-03-01T10:00:00Z').getFullYear() == 2024 && timestamp('2024-03-01T10:00:00Z').getMonth() == 2");
    t("optional.of(1).value() == 1 && optional.none().orValue(2) == 2 && optional.none().or(optional.of(3)).value() == 3");
    t("[1, 2].first().value() == 1 && [].last().hasValue() == false");
    t("type(1) == int && type('a') == string && string(1.5) == '1.5' && int('42') == 42 && bool('true')");
    t("strings.quote('a\"b') == '\"a\\\\\"b\"'");
}

fn decl(schema: Json) -> std::rc::Rc<check::Decl> {
    check::Decl::from_schema(&schema, false, "selfType1").unwrap()
}

fn check_rule(rule: &str, schema: Json) -> Result<check::Checked, String> {
    let d = decl(schema);
    let mut vars = std::collections::BTreeMap::new();
    vars.insert("self".to_string(), check::Var { ty: d.ty(), decl: Some(d.clone()) });
    vars.insert("oldSelf".to_string(), check::Var { ty: d.ty(), decl: Some(d) });
    let p = parse::parse(rule).map_err(|e| e.render(rule))?;
    check::check(&p, &vars)
}

#[test]
fn type_checks() {
    let schema = json!({"type": "object", "properties": {
        "s": {"type": "string", "maxLength": 10}, "i": {"type": "integer"}, "d": {"type": "number"},
        "o": {"type": "object", "properties": {"x": {"type": "string"}}},
        "l": {"type": "array", "maxItems": 5, "items": {"type": "string", "maxLength": 5}},
        "m": {"type": "object", "additionalProperties": {"type": "string"}},
        "ios": {"x-kubernetes-int-or-string": true}, "my-key": {"type": "string"}, "namespace": {"type": "string"},
    }});
    let ok = |r: &str| assert!(check_rule(r, schema.clone()).is_ok(), "{r}: {:?}", check_rule(r, schema.clone()).err());
    let bad = |r: &str, msg: &str| {
        let e = check_rule(r, schema.clone()).err().unwrap_or_default();
        assert!(e.contains(msg), "{r}: {e}");
    };
    ok("self.o.x == 'a'");
    ok("self.o == null");
    ok("self.i < 1.0");
    ok("self.ios == 1 || self.ios == 'a'");
    ok("has(self.o.x) && has(self.m.x) && self.m.x == 'a'");
    ok("self.__namespace__ == 'a' && self.my__dash__key == 'a'");
    ok("self.l.map(x, x.size()).sum() > 0 && self.l.exists_one(x, x == 'a')");
    ok("self.l.exists(x, x == 'a') && self.?o.x.orValue('') == ''");
    ok("type(self.o) == string && dyn(self.i) == 1.0 && 'a' in self.m && self.i in [1, 2]");
    ok("self.l.transformList(i, v, v) == [] && self.m.transformMap(k, v, v) == {}");
    bad("self.foo > 0", "ERROR: <input>:1:5: undefined field 'foo'\n | self.foo > 0\n | ....^");
    bad("self.i == 'a'", "1:8: found no matching overload for '_==_' applied to '(int, string)'");
    bad("self.i >", "Syntax error: mismatched input '<EOF>'");
    bad("self.s.matches('[')", "1:16: invalid matches argument");
    bad("nosuchfn(self.i)", "1:9: undeclared reference to 'nosuchfn' (in container '')");
    bad("self.m == null", "applied to '(map(string, string), null)'");
    bad("self.i == 1.0", "applied to '(int, double)'");
    bad("[1, 'a'].size() == 2", "1:5: expected type 'int' but found 'string'");
    bad("self.l.all(x, x)", "1:15: expected type 'bool' but found 'string'");
    bad("true ? 1 : 'a'", "1:6: found no matching overload for '_?_:_' applied to '(bool, int, string)'");
    bad("self.l.sum() == 0", "found no matching overload for 'sum' applied to 'list(string).()'");
    bad("self.i.size() == 1", "1:12: found no matching overload for 'size' applied to 'int.()'");
    bad("!self.i", "1:1: found no matching overload for '!_' applied to '(int)'");
    bad("self.l + [1] == self.l", "applied to '(list(string), list(int))'");
    bad("math.greatest(1, 2) == 2", "1:1: undeclared reference to 'math'");
    bad("cel.bind(x, 1, x > 0)", "undeclared reference to 'cel'");
    bad("self.o['x'] == 'a'", "found no matching overload for '_[_]' applied to '(selfType1.o, string)'");
}

#[test]
fn costs() {
    let cost = |rule: &str, schema: Json| {
        let c = check_rule(rule, schema).unwrap();
        cost::estimate(&parse::parse(rule).unwrap(), &c).max
    };
    // Unbounded nested loops blow the 1e7 budget; bounded ones do not.
    let unbounded = json!({"type": "array", "items": {"type": "string"}});
    assert!(cost("self.all(x, self.all(y, x == y))", unbounded.clone()) > 1_000_000_000);
    let bounded = json!({"type": "array", "maxItems": 10, "items": {"type": "string", "maxLength": 20}});
    assert_eq!(cost("self.all(t, t.size() > 1)", bounded.clone()), 62);
    assert!(cost("self.all(x, self.all(y, x == y))", bounded) < 10_000);
    // string(int) has no size bound, so concatenating it is unbounded.
    let obj = json!({"type": "object", "properties": {"min": {"type": "integer"}, "mode": {"type": "string", "maxLength": 10}}});
    assert!(cost("'min ' + string(self.min)", obj.clone()) > 1_000_000_000);
    assert!(cost("'mode ' + self.mode + ' wrong'", obj) < 100);
}
