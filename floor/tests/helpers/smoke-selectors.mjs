import ts from 'typescript';

const ANY = '\u0000';
const parse = (text, file = 'driver.mjs') => ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, file.endsWith('tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.JS);
const walk = (node, visit) => { visit(node); ts.forEachChild(node, (child) => walk(child, visit)); };
const unique = (items) => [...new Set(items)];
const product = (left, right) => left.flatMap((a) => right.map((b) => a + b));

function literals(node, bindings = new Map(), seen = new Set()) {
  if (!node || seen.has(node)) return [ANY];
  seen = new Set([...seen, node]);
  if (ts.isStringLiteralLike(node)) return [node.text];
  if (ts.isParenthesizedExpression(node) || ts.isAsExpression(node)) return literals(node.expression, bindings, seen);
  if (ts.isIdentifier(node)) return bindings.has(node.text) ? bindings.get(node.text) : [ANY];
  if (ts.isTemplateExpression(node)) return node.templateSpans.reduce((out, span) =>
    product(out, literals(span.expression, bindings, seen).map((value) => value + span.literal.text)), [node.head.text]);
  if (ts.isConditionalExpression(node)) return unique([...literals(node.whenTrue, bindings, seen), ...literals(node.whenFalse, bindings, seen)]);
  if (ts.isBinaryExpression(node)) {
    const left = literals(node.left, bindings, seen), right = literals(node.right, bindings, seen);
    if (node.operatorToken.kind === ts.SyntaxKind.PlusToken) return product(left, right);
    return unique([...left, ...right]);
  }
  return [ANY];
}

export function driverSelectors(text) {
  const ast = parse(text), bindings = new Map(), selectors = new Set(), tables = new Set(), tableRows = new Set();
  walk(ast, (node) => {
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.initializer && ts.isArrayLiteralExpression(node.initializer)
      && node.initializer.elements.some((item) => ts.isObjectLiteralExpression(item) && item.properties.some((prop) => ts.isPropertyAssignment(prop) && prop.name.getText(ast) === 'selector'))) tables.add(node.name.text);
  });
  walk(ast, (node) => {
    if (ts.isForOfStatement(node) && ts.isArrayLiteralExpression(node.expression)) {
      const name = node.initializer.declarations?.[0]?.name?.text;
      if (name) bindings.set(name, node.expression.elements.flatMap((item) => literals(item)));
    }
    if (ts.isForOfStatement(node) && ts.isIdentifier(node.expression) && tables.has(node.expression.text)) tableRows.add(node.initializer.declarations?.[0]?.name?.text);
    if (ts.isVariableDeclaration(node) && node.initializer && ts.isCallExpression(node.initializer)
      && ts.isPropertyAccessExpression(node.initializer.expression) && node.initializer.expression.name.text === 'find'
      && tables.has(node.initializer.expression.expression.getText(ast))) tableRows.add(node.name.getText(ast));
    if (ts.isPropertyAssignment(node) && node.name.getText(ast) === 'selector') {
      for (const value of literals(node.initializer)) selectors.add(value);
    }
  });
  for (let pass = 0; pass < 3; pass += 1) walk(ast, (node) => {
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.initializer) {
      const values = literals(node.initializer, bindings);
      if (values.some((value) => value !== ANY)) bindings.set(node.name.text, values);
    }
  });
  function add(node) {
    const values = literals(node, bindings);
    if (values.every((value) => value === ANY)) throw new Error(`unsupported driver selector: ${node.getText(ast)}`);
    for (const value of values) selectors.add(value);
  }
  walk(ast, (node) => {
    if (ts.isCallExpression(node)) {
      const name = ts.isPropertyAccessExpression(node.expression) ? node.expression.name.text : node.expression.getText(ast);
      if (['click', 'fill', 'clickButton', 'querySelector', 'querySelectorAll'].includes(name)) {
        const argument = node.arguments[name === 'clickButton' ? 1 : 0];
        if (!argument) selectors.add('button');
        else if (ts.isPropertyAccessExpression(argument) && argument.name.text === 'selector') {
          if (!tableRows.has(argument.expression.getText(ast))) throw new Error(`unsupported selector table row: ${argument.getText(ast)}`);
        } else add(argument);
      }
    }
    if (!ts.isStringLiteralLike(node) && !ts.isTemplateExpression(node)) return;
    if (!node.getText(ast).includes('querySelector')) return;
    let expressions = literals(node, bindings);
    if (ts.isTemplateExpression(node)) {
      expressions = node.templateSpans.reduce((out, span) => {
        const expression = span.expression;
        let values = literals(expression, bindings);
        if (ts.isCallExpression(expression) && expression.expression.getText(ast) === 'JSON.stringify') {
          const input = expression.arguments[0];
          if (['selector', 'scope'].includes(input.getText(ast)) && values[0] === ANY) {
            values = literals(input, bindings);
            if (values.every((value) => value === ANY)) {
              let owner = node.parent;
              while (owner && !ts.isFunctionLike(owner)) owner = owner.parent;
              if (!owner?.parameters.some((parameter) => parameter.name.getText(ast) === input.getText(ast))) throw new Error(`unbound selector: ${input.getText(ast)}`);
              values = ['__forwarded_selector__'];
            }
          } else values = literals(input, bindings);
          values = values.map((value) => JSON.stringify(value));
        }
        return product(out, values.map((value) => value + span.literal.text));
      }, [node.head.text]);
    }
    for (const expression of expressions) {
      const embedded = parse(expression);
      walk(embedded, (call) => {
        if (!ts.isCallExpression(call) || !ts.isPropertyAccessExpression(call.expression)
          || !['querySelector', 'querySelectorAll'].includes(call.expression.name.text)) return;
        const values = literals(call.arguments[0]);
        if (values.every((value) => value === ANY)) throw new Error(`unsupported embedded selector: ${call.getText(embedded)}`);
        for (const value of values) if (value !== '__forwarded_selector__') selectors.add(value);
      });
    }
  });
  return [...selectors];
}

export function selectorParts(selector) {
  const parts = [];
  let rest = selector.trim();
  while (rest) {
    const tokens = [];
    const tag = /^[a-z][a-z0-9-]*/.exec(rest);
    if (tag) { tokens.push(['tag', '=', tag[0]]); rest = rest.slice(tag[0].length); }
    while (rest && !/^\s/.test(rest)) {
      const short = /^([.#])([\w-]+)/.exec(rest);
      const attribute = /^\[([\w-]+)(?:([*$^]?=)["']([^"']*)["'])?\]/.exec(rest);
      if (short) { tokens.push([short[1] === '.' ? 'class' : 'id', short[1] === '.' ? '~=' : '=', short[2]]); rest = rest.slice(short[0].length); }
      else if (attribute) { tokens.push([attribute[1], attribute[2] ?? 'exists', attribute[3] ?? '']); rest = rest.slice(attribute[0].length); }
      else throw new Error(`unsupported selector grammar: ${selector}`);
    }
    if (!tokens.length) throw new Error(`empty selector: ${selector}`);
    parts.push(tokens);
    rest = rest.trimStart();
  }
  return parts;
}

function matchesValue(candidate, operator, wanted) {
  if (operator === 'exists') return true;
  if (candidate === ANY) return false;
  if (operator === '~=') return candidate.split(/\s+/).includes(wanted);
  const escape = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const pattern = candidate.split(ANY).map(escape).join('.*');
  const wantedPattern = wanted.split(ANY).map(escape).join('.*');
  if (wanted.includes(ANY)) return new RegExp(`^${wantedPattern}$`).test(candidate.replaceAll(ANY, 'value'));
  if (operator === '=') return new RegExp(`^${pattern}$`).test(wanted);
  if (operator === '$=') return candidate.endsWith(wanted) || candidate.endsWith(ANY);
  if (operator === '^=') return candidate.startsWith(wanted) || candidate.startsWith(ANY);
  return candidate.includes(wanted) || candidate.includes(ANY);
}

export function renderedElements(files) {
  const sources = new Map([...files].map(([path, text]) => [path, parse(text, path)]));
  const definitions = new Map(), constants = new Map(), elements = [];
  for (const ast of sources.values()) walk(ast, (node) => {
    if (ts.isFunctionDeclaration(node) && node.name) definitions.set(node.name.text, node);
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.initializer) constants.set(node.name.text, node.initializer);
  });
  function values(node, env, seen = new Set()) {
    if (!node || seen.has(node)) return [ANY];
    const next = new Set([...seen, node]);
    if (ts.isIdentifier(node) && !env.has(node.text) && constants.has(node.text)) return values(constants.get(node.text), env, next);
    if (ts.isCallExpression(node)) {
      const name = node.expression.getText();
      if (name === 'cn') return [node.arguments.flatMap((arg) => values(arg, env, next)).join(' ')];
      const fn = definitions.get(name);
      if (fn && fn.parameters.every((parameter) => ts.isIdentifier(parameter.name))) {
        const bound = new Map(env);
        fn.parameters.forEach((parameter, index) => bound.set(parameter.name.text, values(node.arguments[index], env, next)));
        const returned = fn.body?.statements.find(ts.isReturnStatement)?.expression;
        if (returned) return values(returned, bound, next);
      }
    }
    if (ts.isTemplateExpression(node)) return node.templateSpans.reduce((out, span) =>
      product(out, values(span.expression, env, next).map((value) => value + span.literal.text)), [node.head.text]);
    if (ts.isConditionalExpression(node)) return unique([...values(node.whenTrue, env, next), ...values(node.whenFalse, env, next)]);
    if (ts.isBinaryExpression(node)) {
      const left = values(node.left, env, next), right = values(node.right, env, next);
      return node.operatorToken.kind === ts.SyntaxKind.PlusToken ? product(left, right) : unique([...left, ...right]);
    }
    if (ts.isParenthesizedExpression(node) || ts.isAsExpression(node)) return values(node.expression, env, next);
    if (ts.isPropertyAccessExpression(node) && env.has(node.getText())) return env.get(node.getText());
    return literals(node, env);
  }
  function visit(node, parents, env, stack, children = [], fieldId) {
    if (ts.isJsxExpression(node) && node.expression && ts.isIdentifier(node.expression) && node.expression.text === 'children') {
      for (const child of children) visit(child.node, parents, child.env, stack, [], fieldId);
      return;
    }
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression)
      && node.expression.name.text === 'map' && node.arguments[0] && ts.isArrowFunction(node.arguments[0])) {
      const callback = node.arguments[0];
      let list = constants.get(node.expression.expression.getText());
      if (list && ts.isAsExpression(list)) list = list.expression;
      if (list && ts.isCallExpression(list) && list.expression.getText() === 'Object.freeze') list = list.arguments[0];
      if (list && ts.isArrayLiteralExpression(list) && callback.parameters[0] && ts.isIdentifier(callback.parameters[0].name)) {
        const parameter = callback.parameters[0].name.text;
        for (const item of list.elements) {
          const bound = new Map(env);
          if (ts.isObjectLiteralExpression(item)) {
            for (const prop of item.properties) if (ts.isPropertyAssignment(prop)) bound.set(`${parameter}.${prop.name.getText()}`, values(prop.initializer, env));
          } else bound.set(parameter, values(item, env));
          visit(callback.body, parents, bound, stack, children, fieldId);
        }
        return;
      }
    }
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression)
      && node.expression.name.text === 'map' && node.arguments[0] && ts.isIdentifier(node.arguments[0])) {
      const callback = constants.get(node.arguments[0].text);
      if (callback && ts.isArrowFunction(callback)) visit(callback.body, parents, env, stack, children, fieldId);
    }
    if (!ts.isJsxElement(node) && !ts.isJsxSelfClosingElement(node)) {
      ts.forEachChild(node, (child) => visit(child, parents, env, stack, children, fieldId));
      return;
    }
    const opening = ts.isJsxElement(node) ? node.openingElement : node;
    const name = opening.tagName.getText(), attrs = new Map();
    for (const attribute of opening.attributes.properties) {
      if (ts.isJsxSpreadAttribute(attribute)) {
        const spread = env.get(`...${attribute.expression.getText()}`);
        if (spread instanceof Map) for (const [key, value] of spread) attrs.set(key, value);
      } else {
        const key = attribute.name.text === 'className' ? 'class' : attribute.name.text;
        const initializer = attribute.initializer;
        attrs.set(key, initializer ? values(ts.isJsxExpression(initializer) ? initializer.expression : initializer, env) : ['true']);
      }
    }
    const ownChildren = ts.isJsxElement(node) ? node.children.map((child) => ({ node: child, env })) : [];
    if (/^[a-z]/.test(name)) {
      const id = opening.attributes.properties.find((attribute) => ts.isJsxAttribute(attribute) && attribute.name.text === 'id');
      if (fieldId && ['input', 'textarea', 'select'].includes(name) && id?.initializer?.getText().includes('field.id')
        && attrs.get('id')?.includes(ANY)) attrs.set('id', fieldId);
      attrs.set('tag', [name]);
      const element = { attrs, parents };
      elements.push(element);
      for (const child of ownChildren) visit(child.node, [...parents, element], child.env, stack, children, fieldId);
      if (!ownChildren.length && opening.attributes.properties.some(ts.isJsxSpreadAttribute)) {
        for (const child of children) visit(child.node, [...parents, element], child.env, stack, [], fieldId);
      }
      return;
    }
    if (name === 'FieldContext') {
      for (const child of ownChildren) visit(child.node, parents, child.env, stack, children, env.get('id'));
      return;
    }
    const definition = definitions.get(name);
    if (!definition) {
      for (const child of ownChildren) visit(child.node, parents, child.env, stack, children, fieldId);
      return;
    }
    if (stack.includes(name)) return;
    const bound = new Map();
    const parameter = definition.parameters[0]?.name;
    if (parameter && ts.isObjectBindingPattern(parameter)) {
      const rest = new Map(attrs);
      for (const binding of parameter.elements) {
        const key = binding.propertyName?.getText() ?? binding.name.getText();
        const attribute = key === 'className' ? 'class' : key;
        if (binding.dotDotDotToken) bound.set(`...${binding.name.getText()}`, rest);
        else {
          bound.set(binding.name.getText(), attrs.get(attribute) ?? values(binding.initializer, env));
          rest.delete(attribute);
        }
      }
      for (const [key, value] of attrs) bound.set(`rest.${key}`, value);
    }
    visit(definition.body, parents, bound, [...stack, name], ownChildren, fieldId);
  }
  for (const [name, definition] of definitions) visit(definition.body, [], new Map(), [name]);
  return elements;
}

export function missingSelectors(selectors, elements) {
  const matches = (element, tokens) => tokens.every(([name, operator, wanted]) =>
    element.attrs.get(name)?.some((candidate) => (name !== 'id' || !candidate.includes(ANY)) && ((candidate === ANY && (name.startsWith('data-') || name === 'value')) || matchesValue(candidate, operator, wanted))));
  return selectors.filter((selector) => {
    const parts = selectorParts(selector);
    return !elements.some((element) => {
      if (!matches(element, parts.at(-1))) return false;
      let index = parts.length - 2;
      for (const parent of [...element.parents].reverse()) if (index >= 0 && matches(parent, parts[index])) index -= 1;
      return index < 0;
    });
  });
}
