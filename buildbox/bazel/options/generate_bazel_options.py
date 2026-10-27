"""Writes the table of Bazel's options that bazel_options checks command lines against.

Usage: python3 generate_bazel_options.py BAZEL_JAVA_SOURCES HELP_DIR > bazel_options.txt

BAZEL_JAVA_SOURCES is src/main/java in the source of the Bazel release bb
imitates; HELP_DIR holds `bazel help <command> --long` for every command, as
help-<command>.txt. Documented options take the commands whose help lists
them; undocumented ones take those of the documented options declared beside
them, or of the module that registers them.
"""
import os
import re
import sys

FIELD = re.compile(r'\)(?:\s|//[^\n]*\n|@[\w.]+(?:\([^)]*\))?)*public\s+(?:final\s+)?([\w<>\[\]., ?]+?)\s+(\w+)\s*[;=]')
HELP_OPTION = re.compile(r'  --(?:\[no\])?([\w:\-]+)')
STARTUP_CLASSES = {'BlazeServerStartupOptions', 'BazelStartupOptionsModule', 'HostJvmStartupOptions', 'RemoteStartupOptions', 'ClientOptions'}
BUILD_MODULES = {'ExecutionGraphModule', 'IncludeScanningOptions', 'StarlarkDebuggerOptions', 'ConfigFeatureFlagOptions', 'BazelAndroidConfiguration'}
ALL_MODULES = {'PackageMetricsModule', 'PostGCMemoryUseRecorder'}


def attribute(body, key):
    match = re.search(key + r'\s*=\s*((?:"(?:[^"\\]|\\.)*"\s*\+?\s*)+|[\w.]+)', body)
    if not match:
        return None
    value = match.group(1)
    if not value.startswith('"'):
        return None if value == 'null' else value
    return ''.join(re.findall(r'"((?:[^"\\]|\\.)*)"', value)).encode().decode('unicode_escape')


def kind(body, field_type):
    if field_type in ('boolean', 'Boolean'):
        return 'bool'
    if field_type == 'TriState':
        return 'tristate'
    if field_type == 'Void':
        return 'void'
    if 'allowMultiple = true' in body:
        return 'multi'
    return 'value'


def annotated_fields(text):
    """Each `@Option(...)` field in a Java file: where it starts, its annotation's body, and its type."""
    chunks = text.split('@Option(')
    offset = len(chunks[0])
    for chunk in chunks[1:]:
        field = FIELD.search(chunk)
        if field:
            yield offset, chunk[:field.start()], field.group(1).strip()
        offset += len('@Option(') + len(chunk)


def string_constants(source):
    """The values of Java `static final` constants, by name, as defaults may name them."""
    constants = {}
    for root, _, files in os.walk(source):
        for name in files:
            if name.endswith('.java'):
                text = open(os.path.join(root, name), encoding='utf-8').read()
                for match in re.finditer(r'static final \w+ (\w+) = "([^"]*)";', text):
                    constants[match.group(1)] = match.group(2)
    return constants


def resolve(default, constants):
    if default is None:
        return None
    return constants.get(default.split('.')[-1], default)


def declared_options(source):
    constants = string_constants(source)
    for root, _, files in sorted(os.walk(source)):
        for name in sorted(files):
            if not name.endswith('.java'):
                continue
            text = open(os.path.join(root, name), encoding='utf-8').read()
            group = name[:-5]
            graveyard = text.find('class AllCommandGraveyardOptions')
            for start, body, field_type in annotated_fields(text):
                option = attribute(body, 'name')
                if not option or ' ' in option:
                    continue
                key = group
                if group == 'BazelRulesModule':
                    key = 'AllGraveyard' if start > graveyard else 'BuildGraveyard'
                option_kind = kind(body, field_type)
                default = resolve(attribute(body, 'defaultValue'), constants)
                yield option, option_kind, default, key, option
                old_name = attribute(body, 'oldName')
                if old_name:
                    yield old_name, option_kind, default, key, option


def documented_commands(help_dir):
    """Each documented option's commands, and its kind and default as its help gives them."""
    commands = {}
    described = {}
    for name in sorted(os.listdir(help_dir)):
        if not (name.startswith('help-') and name.endswith('.txt')):
            continue
        command = name[len('help-'):-len('.txt')]
        for line in open(os.path.join(help_dir, name)):
            match = HELP_OPTION.match(line)
            if match:
                commands.setdefault(match.group(1), set()).add(command)
                described[match.group(1)] = described_kind(line)
    return commands, described


def described_kind(line):
    default = re.search(r'default: "(.*)"\)', line)
    default = default.group(1) if default else None
    if line.startswith('  --[no]'):
        return 'bool', default
    if 'may be used multiple times' in line:
        return 'multi', default
    return 'value', default


def main(source, help_dir):
    documented, described = documented_commands(help_dir)
    options = [option for option in declared_options(source) if option[3] not in STARTUP_CLASSES]
    declared = {option[0] for option in options}
    options += [(name, *described[name], 'Documented', name) for name in documented if name not in declared]
    every_command = set().union(*documented.values())
    build_commands = documented['build']
    by_group = {}
    for name, _, _, group, _ in options:
        if name in documented:
            by_group.setdefault(group, set()).update(documented[name])
    table = {}
    for name, option_kind, default, group, canonical in options:
        commands = documented.get(canonical) or by_group.get(group)
        if not commands and (group in BUILD_MODULES or group == 'BuildGraveyard'):
            commands = build_commands
        if not commands and (group in ALL_MODULES or group == 'AllGraveyard'):
            commands = every_command
        if not commands:
            sys.exit(f'no commands known for --{name} ({group})')
        table[name] = (option_kind, default, frozenset(commands))
    sets = sorted({commands for _, _, commands in table.values()}, key=lambda commands: (-len(commands), sorted(commands)))
    print('# Bazel\'s options, generated by generate_bazel_options.py; don\'t edit.')
    print('# "@N commands" lines define command sets; option lines are')
    print('# "name kind set default", with default "-" for none.')
    for index, commands in enumerate(sets):
        print(f'@{index} ' + ' '.join(sorted(commands)))
    for name in sorted(table):
        option_kind, default, commands = table[name]
        default = '-' if default in (None, 'null') else repr(default)[1:-1] if default else "''"
        print(f'{name} {option_kind} {sets.index(commands)} {default}')


if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
